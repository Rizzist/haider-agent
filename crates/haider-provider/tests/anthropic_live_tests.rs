//! Explicitly gated network smoke and fixture-promotion harness.
//!
//! These tests are ignored by default. The C1 lane must not run them.

#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use haider_accounts::{CredentialAlias, MemoryVault, Vault, import_env};
use haider_protocol::ids::ArtifactRef;
use haider_protocol::provider::{
    Block, FinishReason, PrefixDigests, StreamEvent, Usage, UsageSource,
};
use haider_protocol::tool::AttachmentBlock;
use haider_provider::{
    AnthropicCapture, AnthropicProvider, Message, MessageRole, Provider, ProviderError,
    ProviderErrorKind, ProviderStreamItem, ResolvedAttachment, ToolDefinition, TurnRequest,
    replay_anthropic_http_error, replay_anthropic_sse,
};
use serde::{Deserialize, Serialize};

const LIVE_GATE: &str = "HAIDER_LIVE_PROVIDER_TESTS";
const PROMOTE_GATE: &str = "HAIDER_PROMOTE_ANTHROPIC_FIXTURES";
const KEY_ENV: &str = "HAIDER_ANTHROPIC_API_KEY";
const MODEL_ENV: &str = "HAIDER_ANTHROPIC_MODEL";
const RATE_LIMIT_URL_ENV: &str = "HAIDER_ANTHROPIC_CAPTURE_429_URL";
const MALFORMED_URL_ENV: &str = "HAIDER_ANTHROPIC_CAPTURE_MALFORMED_URL";
const OVERLOAD_URL_ENV: &str = "HAIDER_ANTHROPIC_CAPTURE_OVERLOAD_URL";
const REQUIRED_CAPTURE_SHAPES: [&str; 7] = [
    "text_only",
    "tool_call",
    "image_in",
    "usage_heavy",
    "rate_limit",
    "malformed",
    "overloaded_stream",
];

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
#[ignore = "live Anthropic cache assertion; requires HAIDER_LIVE_PROVIDER_TESTS=1"]
async fn live_anthropic_second_turn_reads_cached_prompt_prefix() {
    if std::env::var(LIVE_GATE).as_deref() != Ok("1") {
        return;
    }
    let vault = import_live_key();
    let model = selected_model();
    let provider = live_provider(&vault, &model);
    // Round 5: the SYSTEM stays tiny and the HISTORY carries the bulk —
    // a fat system alone could satisfy the coverage threshold while the
    // message-history cache silently regressed. With ~150 system tokens
    // and a multi-thousand-token first turn, 50% coverage is impossible
    // without the history riding the cache.
    let stable_system = "Stable cache-test context. Keep every word unchanged. ".repeat(3);
    let fat_history = format!(
        "Background dossier (verbatim, unchanging): {}",
        "the caravan crossed the salt flats at dawn carrying ledgers of amber and tin. "
            .repeat(600)
    );
    let mut first_request = cache_assertion_request(
        &model,
        stable_system.clone(),
        vec![
            Message::user_text(fat_history),
            Message::user_text("Reply with exactly: cache-turn-one"),
        ],
        1,
        1,
    );

    let (first_reply, first_usage) = run_live_cache_turn(&provider, first_request.clone()).await;
    let first_normalized = first_usage
        .normalized
        .as_ref()
        .expect("turn 1 reports normalized cache usage");
    assert!(
        first_normalized.cache_write_input > 0,
        "turn 1 must report cache_creation_input_tokens > 0: {first_normalized:?}"
    );
    let prior_prompt_tokens = first_normalized.logical_input;
    assert!(
        prior_prompt_tokens > 2_000,
        "harness: the history must dominate the prompt ({prior_prompt_tokens} tokens)"
    );

    first_request.messages.extend([
        Message::assistant(vec![Block::Text { text: first_reply }]),
        Message::user_text("Reply with exactly: cache-turn-two"),
    ]);
    let metadata = first_request
        .cache_metadata
        .as_mut()
        .expect("cache metadata");
    metadata.stable_history_end = 3;
    metadata.current_user_start = 3;
    let (_, second_usage) = run_live_cache_turn(&provider, first_request).await;
    let second_normalized = second_usage
        .normalized
        .as_ref()
        .expect("turn 2 reports normalized cache usage");
    assert!(
        second_normalized.cache_read_input > 0,
        "turn 2 must report cache_read_input_tokens > 0: {second_normalized:?}"
    );
    assert!(
        second_normalized.cache_read_input >= prior_prompt_tokens.div_ceil(2),
        "turn 2 cache read ({}) must cover at least 50% of turn 1 prompt ({prior_prompt_tokens})",
        second_normalized.cache_read_input
    );
}

fn cache_assertion_request(
    model: &str,
    system_prompt: String,
    messages: Vec<Message>,
    stable_history_end: usize,
    current_user_start: usize,
) -> TurnRequest {
    TurnRequest {
        messages,
        model: model.into(),
        max_tokens: 32,
        system_prompt: Some(system_prompt),
        tools: vec![ToolDefinition {
            name: "cache_probe".into(),
            description: "A stable no-op tool definition used only to verify prompt caching".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }],
        attachments: Vec::new(),
        cache_metadata: Some(haider_provider::PromptCacheMetadata {
            stable_history_end,
            current_user_start,
            previous_stable_history_end: None,
            latest_compaction_summary_end: None,
            prefix_digests: PrefixDigests {
                system: "live-stable-system".into(),
                tools: "live-stable-tools".into(),
                immutable_history: "live-stable-history".into(),
                model: "live-stable-model".into(),
                auth_mode: "live-api-key".into(),
                reasoning_settings: "live-default-reasoning".into(),
            },
            cache_epoch: "live-cache-assertion-v1".into(),
            compaction_epoch: "live-root-compaction".into(),
            provider: "anthropic".into(),
            session_scope: "live-cache-assertion-session".into(),
            account_scope: Some("anthropic-env".into()),
            stable_prefix_tokens: 5_000,
            expected_later_reads: 2,
            reuse_gap_ms: Some(1_000),
        }),
    }
}

async fn run_live_cache_turn(
    provider: &AnthropicProvider,
    request: TurnRequest,
) -> (String, Usage) {
    let mut stream = provider
        .stream_turn(request)
        .await
        .expect("live cache stream starts");
    let mut text = String::new();
    let mut usage = None;
    let mut saw_finish = false;
    while let Some(item) = stream.recv().await {
        match item.expect("live cache stream item") {
            StreamEvent::TextDelta { text: delta } => text.push_str(&delta),
            StreamEvent::UsageUpdate(update) => usage = Some(update),
            StreamEvent::Finish { .. } => {
                saw_finish = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_finish, "live cache turn must finish");
    assert!(
        !text.is_empty(),
        "live cache turn must return assistant text"
    );
    (text, usage.expect("live cache turn reports usage"))
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
    let overload_url = std::env::var(OVERLOAD_URL_ENV)
        .expect("promotion requires an endpoint that returns a mid-stream overload");
    let vault = import_live_key();
    let capture_secret = vault
        .resolve(&CredentialAlias::new("anthropic-env"))
        .expect("resolves key for capture redaction");
    let model = selected_model();
    let image_ref = ArtifactRef::new("blake3:capture-image");

    let shapes = [
        CaptureShape::new(
            "text_only",
            CaptureSemantics::Text,
            text_request(&model, "Reply in one short sentence."),
            None,
            200,
        ),
        CaptureShape::new(
            "tool_call",
            CaptureSemantics::ToolCall,
            tool_request(&model),
            None,
            200,
        ),
        CaptureShape::new(
            "image_in",
            CaptureSemantics::ImageText,
            image_request(&model, image_ref),
            None,
            200,
        ),
        CaptureShape::new(
            "usage_heavy",
            CaptureSemantics::UsageHeavy,
            usage_request(&model),
            None,
            200,
        ),
        CaptureShape::new(
            "rate_limit",
            CaptureSemantics::RateLimit,
            text_request(&model, "Return the configured rate-limit capture."),
            Some(rate_limit_url),
            429,
        ),
        CaptureShape::new(
            "malformed",
            CaptureSemantics::Malformed,
            text_request(&model, "Return the configured malformed capture."),
            Some(malformed_url),
            200,
        ),
        CaptureShape::new(
            "overloaded_stream",
            CaptureSemantics::OverloadedStream,
            text_request(&model, "Return the configured mid-stream overload."),
            Some(overload_url),
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

    // Build and validate every replacement in memory. A failed semantic gate
    // leaves the entire existing fixture set untouched.
    let mut prepared = Vec::new();
    for (shape, response) in captured {
        prepared.push(prepare_fixture(shape, response));
    }

    let directory = fixture_directory();
    assert_manifest_shapes_preserved(&directory, &prepared);
    for fixture in &prepared {
        write_bytes(&directory.join(&fixture.entry.wire), &fixture.wire);
        write_bytes(&directory.join(&fixture.entry.golden), &fixture.golden);
    }
    let entries = prepared.into_iter().map(|fixture| fixture.entry).collect();
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
    semantics: CaptureSemantics,
    request: TurnRequest,
    api_url: Option<String>,
    expected_status: u16,
}

impl CaptureShape {
    fn new(
        name: &'static str,
        semantics: CaptureSemantics,
        request: TurnRequest,
        api_url: Option<String>,
        expected_status: u16,
    ) -> Self {
        Self {
            name,
            semantics,
            request,
            api_url,
            expected_status,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CaptureSemantics {
    Text,
    ToolCall,
    ImageText,
    UsageHeavy,
    RateLimit,
    Malformed,
    OverloadedStream,
}

struct PreparedFixture {
    entry: ManifestEntry,
    wire: Vec<u8>,
    golden: Vec<u8>,
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

#[derive(Deserialize)]
struct ExistingManifest {
    fixtures: Vec<ExistingManifestEntry>,
}

#[derive(Deserialize)]
struct ExistingManifestEntry {
    name: String,
}

fn prepare_fixture(shape: CaptureShape, response: AnthropicCapture) -> PreparedFixture {
    let CaptureShape {
        name,
        semantics,
        request,
        ..
    } = shape;
    let AnthropicCapture {
        status,
        retry_after,
        body,
    } = response;

    if matches!(semantics, CaptureSemantics::RateLimit) {
        assert_eq!(status, 429, "rate-limit capture must use HTTP 429");
        let error = replay_anthropic_http_error(status, retry_after.as_deref(), &body);
        assert_rate_limit_capture(name, retry_after.as_deref(), &error);
        let wire_name = format!("{name}.http.json");
        let golden_name = format!("{name}.error.json");
        PreparedFixture {
            entry: ManifestEntry {
                name,
                transport: "http",
                status,
                retry_after,
                wire: wire_name,
                golden: golden_name,
            },
            wire: body,
            golden: json_bytes(&error),
        }
    } else {
        assert_eq!(status, 200, "SSE capture `{name}` must use HTTP 200");
        let items = replay_anthropic_sse(&body);
        assert_capture_semantics(name, semantics, &request, &body, &items);
        let wire_name = format!("{name}.sse");
        let golden_name = format!("{name}.events.json");
        PreparedFixture {
            entry: ManifestEntry {
                name,
                transport: "sse",
                status,
                retry_after: None,
                wire: wire_name,
                golden: golden_name,
            },
            wire: body,
            golden: json_bytes(&golden_items(items)),
        }
    }
}

fn assert_capture_semantics(
    name: &str,
    semantics: CaptureSemantics,
    request: &TurnRequest,
    body: &[u8],
    items: &[ProviderStreamItem],
) {
    assert!(!items.is_empty(), "shape `{name}` produced no replay items");
    match semantics {
        CaptureSemantics::Text | CaptureSemantics::ImageText => {
            if matches!(semantics, CaptureSemantics::ImageText) {
                // r2 gate: a text-only capture must NOT be promotable as
                // image_in — the REQUEST must have carried an image.
                assert!(
                    !request.attachments.is_empty(),
                    "shape `{name}` request must carry an image attachment"
                );
            }
            assert_successful_end_turn(name, items);
            assert!(
                items.iter().any(|item| matches!(
                    item,
                    Ok(StreamEvent::TextDelta { text }) if !text.is_empty()
                )),
                "shape `{name}` must emit a nonempty AgentText delta"
            );
        }
        CaptureSemantics::ToolCall => assert_tool_call_capture(name, items),
        CaptureSemantics::UsageHeavy => assert_usage_capture(name, request, body, items),
        CaptureSemantics::OverloadedStream => {
            let message_start = sse_event_offset(body, "message_start")
                .expect("overload capture must contain message_start");
            let error_event =
                sse_event_offset(body, "error").expect("overload capture must contain error");
            assert!(
                message_start < error_event,
                "overload capture must fail after the stream starts"
            );
            assert!(
                matches!(
                    items.last(),
                    Some(Err(error))
                        if error.kind == ProviderErrorKind::Overloaded && error.retryable
                ),
                "overload capture must end with a retryable typed Overloaded error"
            );
        }
        CaptureSemantics::Malformed => {
            assert!(
                matches!(
                    items.last(),
                    Some(Err(error))
                        if error.kind == ProviderErrorKind::MalformedFrame && !error.retryable
                ),
                "malformed capture must end with a non-retryable MalformedFrame"
            );
        }
        CaptureSemantics::RateLimit => {
            panic!("rate-limit semantics require the HTTP validation path")
        }
    }
}

fn assert_successful_end_turn(name: &str, items: &[ProviderStreamItem]) {
    assert!(
        items.iter().all(Result::is_ok),
        "shape `{name}` must not contain a stream error"
    );
    assert!(
        matches!(
            items.last(),
            Some(Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn
            }))
        ),
        "shape `{name}` must end with end_turn"
    );
}

fn assert_tool_call_capture(name: &str, items: &[ProviderStreamItem]) {
    assert!(
        items.iter().all(Result::is_ok),
        "shape `{name}` must not contain a stream error"
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item, Ok(StreamEvent::ToolCallStart { .. })))
            .count(),
        1,
        "tool-call capture must start exactly one call"
    );
    let (call_id, tool_name) = items
        .iter()
        .find_map(|item| match item {
            Ok(StreamEvent::ToolCallStart { call_id, name }) => {
                Some((call_id.as_str(), name.as_str()))
            }
            _ => None,
        })
        .expect("tool-call capture must contain ToolCallStart");
    assert_eq!(tool_name, "get_weather");

    let mut args = String::new();
    for item in items {
        if let Ok(StreamEvent::ToolCallArgsDelta {
            call_id: fragment_call_id,
            args_fragment,
        }) = item
        {
            assert_eq!(
                fragment_call_id, call_id,
                "tool argument fragment must reference the started call"
            );
            args.push_str(args_fragment);
        }
    }
    assert!(!args.is_empty(), "tool-call capture must stream arguments");
    let args_json: serde_json::Value =
        serde_json::from_str(&args).expect("tool-call fragments must concatenate to JSON");
    assert!(
        args_json.is_object(),
        "tool-call arguments must be a JSON object"
    );
    assert!(
        items.iter().any(|item| matches!(
            item,
            Ok(StreamEvent::ToolCallEnd {
                call_id: ended_call_id
            }) if ended_call_id == call_id
        )),
        "tool-call capture must end the started call"
    );
    assert!(
        matches!(
            items.last(),
            Some(Ok(StreamEvent::Finish {
                reason: FinishReason::ToolUse
            }))
        ),
        "tool-call capture must finish with tool_use"
    );
}

fn assert_usage_capture(
    name: &str,
    request: &TurnRequest,
    body: &[u8],
    items: &[ProviderStreamItem],
) {
    // r2 gate: "usage-heavy" must mean a genuinely large prompt — a trivial
    // text capture must not satisfy this shape.
    let request_chars: usize = request
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .map(|block| match block {
            Block::Text { text } => text.len(),
            Block::Reasoning { summary } => summary.len(),
            Block::ToolResult { preview, .. } => preview.len(),
            _ => 0,
        })
        .sum();
    assert!(
        request_chars >= 4096,
        "shape `{name}` request carries only {request_chars} chars — not usage-heavy"
    );
    assert_successful_end_turn(name, items);
    let wire = std::str::from_utf8(body).expect("usage capture must be UTF-8 SSE");
    assert!(
        wire.contains("\"input_tokens\"") && wire.contains("\"output_tokens\""),
        "usage capture must contain input and output usage frame fields"
    );
    let usage = items
        .iter()
        .find_map(|item| match item {
            Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
            _ => None,
        })
        .expect("usage-heavy capture must emit UsageUpdate");
    assert!(
        usage.input >= 1000,
        "usage-heavy capture must report >=1000 input tokens (got {})",
        usage.input
    );
    assert!(usage.output > 0, "usage output tokens must be populated");
    assert_eq!(usage.reasoning, 0);
    assert_eq!(usage.source, UsageSource::ProviderReported);
}

fn assert_rate_limit_capture(name: &str, retry_after: Option<&str>, error: &ProviderError) {
    assert!(
        retry_after.is_some_and(|value| !value.trim().is_empty()),
        "shape `{name}` must capture Retry-After"
    );
    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert!(error.retryable);
    assert!(
        error.retry_after_ms.is_some(),
        "typed rate-limit error must retain Retry-After"
    );
}

fn sse_event_offset(body: &[u8], event: &str) -> Option<usize> {
    let marker = format!("event: {event}");
    body.windows(marker.len())
        .position(|window| window == marker.as_bytes())
}

fn assert_manifest_shapes_preserved(directory: &Path, prepared: &[PreparedFixture]) {
    let manifest_bytes =
        fs::read(directory.join("manifest.json")).expect("reads existing fixture manifest");
    let manifest: ExistingManifest =
        serde_json::from_slice(&manifest_bytes).expect("parses existing fixture manifest");
    let existing_names = manifest
        .fixtures
        .iter()
        .map(|fixture| fixture.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        existing_names.len(),
        manifest.fixtures.len(),
        "existing manifest contains duplicate shape names"
    );

    let prepared_names = prepared
        .iter()
        .map(|fixture| fixture.entry.name.to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        prepared_names.len(),
        prepared.len(),
        "capture plan contains duplicate shape names"
    );
    let required_names = REQUIRED_CAPTURE_SHAPES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        prepared_names, required_names,
        "promotion must capture all seven required shapes, including mid-stream overload"
    );
    assert_eq!(
        existing_names, prepared_names,
        "promotion would add or drop shapes from the existing manifest"
    );
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
        cache_metadata: None,
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
        cache_metadata: None,
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
        cache_metadata: None,
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
        cache_metadata: None,
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
            normalize_sse_eof(&mut capture.body);
        } else if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&capture.body) {
            sanitize_json(&mut value);
            capture.body = serde_json::to_vec_pretty(&value).expect("sanitized body serializes");
        }
    }
    capture
}

fn normalize_sse_eof(bytes: &mut Vec<u8>) {
    while bytes.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
        bytes.pop();
    }
    bytes.push(b'\n');
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
    let bytes = json_bytes(value);
    write_bytes(path, &bytes);
}

fn json_bytes(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serializes promoted fixture");
    bytes.push(b'\n');
    bytes
}

// ---- negative promotion-gate tests (offline; review r2) ----
// A well-formed TEXT capture must NOT be promotable under the image_in or
// usage_heavy shapes: wrong-shape captures may never replace named fixtures.

fn text_capture_for_gate_tests() -> (TurnRequest, Vec<u8>, Vec<ProviderStreamItem>) {
    let body = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic/text_only.sse"),
    )
    .expect("committed text fixture");
    let items = replay_anthropic_sse(&body);
    let request = TurnRequest {
        messages: vec![Message::user_text("small prompt")],
        model: "claude-fable-5".to_owned(),
        max_tokens: 512,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    };
    (request, body, items)
}

#[test]
fn text_capture_cannot_pass_the_image_gate() {
    let (request, body, items) = text_capture_for_gate_tests();
    let outcome = std::panic::catch_unwind(|| {
        assert_capture_semantics(
            "image_in",
            CaptureSemantics::ImageText,
            &request,
            &body,
            &items,
        );
    });
    assert!(
        outcome.is_err(),
        "a text capture without an image attachment must fail the image_in gate"
    );
}

#[test]
fn small_capture_cannot_pass_the_usage_heavy_gate() {
    let (request, body, items) = text_capture_for_gate_tests();
    let outcome = std::panic::catch_unwind(|| {
        assert_capture_semantics(
            "usage_heavy",
            CaptureSemantics::UsageHeavy,
            &request,
            &body,
            &items,
        );
    });
    assert!(
        outcome.is_err(),
        "a small-prompt capture must fail the usage_heavy gate"
    );
}
