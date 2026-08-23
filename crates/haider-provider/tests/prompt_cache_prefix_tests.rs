#![allow(clippy::expect_used)]

use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::provider::{Block, PrefixDigests};
use haider_provider::{
    AnthropicProvider, Message, OpenAiCompatibleProvider, PromptCacheMetadata, ToolDefinition,
    TurnRequest,
};

#[derive(Debug, PartialEq, Eq)]
enum PrefixError {
    NotStrict {
        prefix_len: usize,
        extended_len: usize,
    },
    Diverged {
        index: usize,
    },
}

/// Round 5: `cache_control` markers are request METADATA the provider never
/// hashes — a stable-boundary advance legitimately moves them between
/// turns. Strip them before comparing so the detector pins the TOKEN
/// prefix, not marker placement (breakpoint budgets are asserted
/// separately on the unstripped payloads).
fn without_cache_markers(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(without_cache_markers).collect())
        }
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "cache_control")
                .map(|(key, inner)| (key.clone(), without_cache_markers(inner)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn assert_serialized_strict_prefix(
    prefix: &[serde_json::Value],
    extended: &[serde_json::Value],
) -> Result<(), PrefixError> {
    if prefix.len() >= extended.len() {
        return Err(PrefixError::NotStrict {
            prefix_len: prefix.len(),
            extended_len: extended.len(),
        });
    }
    for (index, (left, right)) in prefix.iter().zip(extended).enumerate() {
        let left = without_cache_markers(left);
        let right = without_cache_markers(right);
        if serde_json::to_vec(&left).expect("serialize prefix element")
            != serde_json::to_vec(&right).expect("serialize extended element")
        {
            return Err(PrefixError::Diverged { index });
        }
    }
    Ok(())
}

fn count_breakpoints(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => values.iter().map(count_breakpoints).sum(),
        serde_json::Value::Object(object) => {
            usize::from(object.contains_key("cache_control"))
                + usize::from(object.contains_key("prompt_cache_breakpoint"))
                + object.values().map(count_breakpoints).sum::<usize>()
        }
        _ => 0,
    }
}

fn credential(alias: &str) -> haider_accounts::SecretHandle {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new(alias);
    vault
        .put(&alias, b"prompt-cache-prefix-test-key")
        .expect("store test credential");
    vault.resolve(&alias).expect("resolve test credential")
}

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "read_weather".into(),
        description: "Read the current weather for a city".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }]
}

fn cache_metadata(
    provider: &str,
    stable_history_end: usize,
    current_user_start: usize,
) -> PromptCacheMetadata {
    PromptCacheMetadata {
        stable_history_end,
        current_user_start,
        previous_stable_history_end: None,
        latest_compaction_summary_end: Some(1),
        prefix_digests: PrefixDigests {
            system: "stable-system".into(),
            tools: "stable-tools".into(),
            immutable_history: "stable-history".into(),
            model: "stable-model".into(),
            auth_mode: "stable-auth".into(),
            reasoning_settings: "stable-reasoning".into(),
        },
        cache_epoch: "cache-epoch-a".into(),
        compaction_epoch: "compaction-epoch-a".into(),
        provider: provider.into(),
        session_scope: "prefix-session".into(),
        account_scope: Some("prefix-account".into()),
        stable_prefix_tokens: 4_096,
        expected_later_reads: 2,
        reuse_gap_ms: Some(1_000),
    }
}

fn turn_n(model: &str, provider: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![
            Message::user_text("Stable compacted conversation summary."),
            Message::user_text("What is the weather in Tehran?"),
        ],
        model: model.into(),
        max_tokens: 64,
        system_prompt: Some("You are a concise weather assistant.".into()),
        tools: tools(),
        attachments: Vec::new(),
        cache_metadata: Some(cache_metadata(provider, 1, 1)),
    }
}

fn append_next_turn(request: &mut TurnRequest) {
    request.messages.extend([
        Message::assistant(vec![Block::ToolCall {
            call_id: "call-weather-1".into(),
            name: "read_weather".into(),
            args: serde_json::json!({"city": "Tehran"}),
        }]),
        Message::tool_result("call-weather-1", "Sunny, 27 C", false),
        Message::user_text("Should I bring a jacket?"),
    ]);
    let metadata = request.cache_metadata.as_mut().expect("cache metadata");
    metadata.stable_history_end = 4;
    metadata.current_user_start = 4;
}

fn payload_messages(payload: &serde_json::Value) -> &[serde_json::Value] {
    payload["messages"]
        .as_array()
        .expect("payload messages array")
}

#[test]
fn anthropic_append_only_turn_preserves_wire_prefix_and_breakpoint_limit() {
    let provider = AnthropicProvider::new(credential("anthropic-prefix"), "claude-sonnet-5")
        .expect("construct Anthropic provider");
    let first_request = turn_n("claude-sonnet-5", "anthropic");
    let mut next_request = first_request.clone();
    append_next_turn(&mut next_request);

    let first = provider
        .request_payload(&first_request)
        .expect("render turn N");
    let next = provider
        .request_payload(&next_request)
        .expect("render turn N+1");

    let turn_n_golden = serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 64,
        "messages": [
            {
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "Stable compacted conversation summary.",
                    "cache_control": {"type": "ephemeral", "ttl": "5m"}
                }]
            },
            {
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "What is the weather in Tehran?"
                }]
            }
        ],
        "stream": true,
        "system": [{
            "type": "text",
            "text": "You are a concise weather assistant.",
            "cache_control": {"type": "ephemeral", "ttl": "5m"}
        }],
        "tools": [{
            "name": "read_weather",
            "description": "Read the current weather for a city",
            "input_schema": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            },
            "cache_control": {"type": "ephemeral", "ttl": "5m"}
        }]
    });
    assert_eq!(first, turn_n_golden, "turn N wire golden changed");
    assert_eq!(
        serde_json::to_vec(&first["tools"]).expect("serialize turn N tools"),
        serde_json::to_vec(&next["tools"]).expect("serialize turn N+1 tools")
    );
    assert_eq!(
        serde_json::to_vec(&first["system"]).expect("serialize turn N system"),
        serde_json::to_vec(&next["system"]).expect("serialize turn N+1 system")
    );
    assert_eq!(
        assert_serialized_strict_prefix(payload_messages(&first), payload_messages(&next)),
        Ok(())
    );
    assert!(count_breakpoints(&first) <= 4, "turn N: {first}");
    assert!(count_breakpoints(&next) <= 4, "turn N+1: {next}");
}

#[test]
fn anthropic_prefix_detector_reports_early_message_divergence() {
    let provider = AnthropicProvider::new(credential("anthropic-negative"), "claude-sonnet-5")
        .expect("construct Anthropic provider");
    let first_request = turn_n("claude-sonnet-5", "anthropic");
    let mut mutated_next_request = first_request.clone();
    append_next_turn(&mut mutated_next_request);
    let Block::Text { text } = &mut mutated_next_request.messages[0].blocks[0] else {
        panic!("early message is text")
    };
    text.replace_range(..1, "s");

    let first = provider
        .request_payload(&first_request)
        .expect("render turn N");
    let mutated_next = provider
        .request_payload(&mutated_next_request)
        .expect("render mutated turn N+1");

    assert_eq!(
        assert_serialized_strict_prefix(payload_messages(&first), payload_messages(&mutated_next)),
        Err(PrefixError::Diverged { index: 0 })
    );
}

#[test]
fn openai_compatible_append_only_turn_preserves_wire_prefix() {
    let provider = OpenAiCompatibleProvider::new(
        credential("openai-compatible-prefix"),
        "test-chat-model",
        "https://example.com/v1",
    )
    .expect("construct OpenAI-compatible provider");
    let first_request = turn_n("test-chat-model", "openai-compatible");
    let mut next_request = first_request.clone();
    append_next_turn(&mut next_request);

    let first = provider
        .request_payload(&first_request)
        .expect("render compatible turn N");
    let next = provider
        .request_payload(&next_request)
        .expect("render compatible turn N+1");
    let first_messages = payload_messages(&first);
    let next_messages = payload_messages(&next);

    assert_eq!(
        serde_json::to_vec(&first["tools"]).expect("serialize turn N tools"),
        serde_json::to_vec(&next["tools"]).expect("serialize turn N+1 tools")
    );
    assert_eq!(
        serde_json::to_vec(&first_messages[0]).expect("serialize turn N system message"),
        serde_json::to_vec(&next_messages[0]).expect("serialize turn N+1 system message")
    );
    assert_eq!(
        assert_serialized_strict_prefix(first_messages, next_messages),
        Ok(())
    );
    assert!(count_breakpoints(&first) <= 4, "turn N: {first}");
    assert!(count_breakpoints(&next) <= 4, "turn N+1: {next}");
}
