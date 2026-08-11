#![allow(clippy::expect_used)]

use std::future;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::provider::{Block, PrefixDigests, StreamEvent};
use reqwest::header::AUTHORIZATION;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::anthropic::{
    ANTHROPIC_FAST_BETA_VALUE, ANTHROPIC_OAUTH_BASE_URL, ANTHROPIC_OAUTH_BETA_HEADER,
    ANTHROPIC_OAUTH_BETA_VALUE, ANTHROPIC_OAUTH_SYSTEM_IDENTITY, AnthropicProvider, SseChunkSource,
    replay_anthropic_sse, stream_sse_source,
};
use crate::origin::FixedDnsResolver;
use crate::{
    AnthropicCacheTtl, Message, PromptCacheMetadata, Provider as _, ProviderError,
    ProviderErrorKind, ToolDefinition, TurnRequest, select_anthropic_cache_ttl,
};

struct HangingFixture {
    first_chunk: Option<Vec<u8>>,
}

struct StubFixedResolver {
    address: SocketAddr,
}

#[async_trait]
impl FixedDnsResolver for StubFixedResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![self.address])
    }
}

#[test]
fn anthropic_credential_client_ignores_inherited_proxy_environment() {
    const CHILD_MARKER: &str = "HAIDER_ANTHROPIC_PROXY_PIN_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let vault = MemoryVault::new();
        let alias = CredentialAlias::new("anthropic-proxy-audit");
        vault
            .put(&alias, b"anthropic-proxy-sentinel")
            .expect("store proxy audit secret");
        let credential = vault.resolve(&alias).expect("resolve proxy audit secret");
        let provider =
            AnthropicProvider::new(credential, "claude-audit").expect("Anthropic client");
        assert!(
            !provider.client_debug().contains("proxies"),
            "Anthropic credential-bearing client retained inherited proxy configuration"
        );
        let subscription_vault = MemoryVault::new();
        let subscription_alias = CredentialAlias::new("anthropic-subscription-proxy-audit");
        subscription_vault
            .put(
                &subscription_alias,
                b"anthropic-subscription-proxy-sentinel",
            )
            .expect("store subscription proxy audit secret");
        let subscription = AnthropicProvider::new_subscription(
            subscription_vault
                .resolve(&subscription_alias)
                .expect("resolve subscription proxy secret"),
            "claude-audit",
            ANTHROPIC_OAUTH_BASE_URL,
        )
        .expect("Anthropic subscription client");
        assert!(
            !subscription.client_debug().contains("proxies"),
            "Anthropic subscription client retained inherited proxy configuration"
        );
        return;
    }

    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .arg("anthropic_credential_client_ignores_inherited_proxy_environment")
        .arg("--nocapture")
        .env(CHILD_MARKER, "1")
        .env("HTTP_PROXY", "http://127.0.0.1:18080")
        .env("HTTPS_PROXY", "http://127.0.0.1:18080")
        .env("ALL_PROXY", "http://127.0.0.1:18080")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run isolated Anthropic proxy child");
    assert!(
        output.status.success(),
        "Anthropic proxy child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// MUTATION CHECK: restore x-api-key, drop the OAuth beta, change Bearer, or
/// accept a private subscription origin, or disconnect the pinned resolver.
/// The named assertions fail without sending a credential-bearing request.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn anthropic_oauth_subscription_is_bearer_beta_without_api_key_and_fixed_origin() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-oauth-request-audit");
    vault
        .put(&alias, b"ANTHROPIC_OAUTH_ACCESS_SENTINEL_591c")
        .expect("store OAuth access");
    let provider = AnthropicProvider::new_subscription_with_dns_resolver(
        vault.resolve(&alias).expect("resolve OAuth access"),
        "claude-audit",
        ANTHROPIC_OAUTH_BASE_URL,
        Arc::new(StubFixedResolver {
            address: SocketAddr::from(([93, 184, 216, 34], 443)),
        }),
    )
    .expect("Anthropic subscription provider");
    let request = provider
        .request(&serde_json::json!({"model":"claude-audit"}))
        .await
        .expect("fixed request");
    assert_eq!(
        request.url().as_str(),
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(
        request.headers().get(AUTHORIZATION).expect("Bearer header"),
        "Bearer ANTHROPIC_OAUTH_ACCESS_SENTINEL_591c"
    );
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .expect("OAuth beta header"),
        ANTHROPIC_OAUTH_BETA_VALUE
    );
    assert_eq!(
        request
            .headers()
            .get("anthropic-version")
            .expect("Anthropic version"),
        "2023-06-01"
    );
    assert!(!request.headers().contains_key("x-api-key"));
    assert!(
        provider.stall_fixed_connection_resolution(),
        "subscription provider has a fixed-origin guard"
    );
    let execution = provider.execute_request_for_test(request);
    tokio::pin!(execution);
    let resolution_observed = async {
        while provider.fixed_connection_resolution_count() == Some(0) {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        result = &mut execution => {
            panic!("fixed connection resolver did not stall the request: {result:?}");
        }
        observed = tokio::time::timeout(Duration::from_secs(1), resolution_observed) => {
            observed.expect("reqwest must consume the pinned fixed resolver");
        }
    }
    assert_eq!(
        provider.fixed_connection_resolution_count(),
        Some(1),
        "one connection lookup must use the pinned fixed resolver"
    );

    let private_vault = MemoryVault::new();
    let private_alias = CredentialAlias::new("anthropic-private-origin-audit");
    private_vault
        .put(&private_alias, b"NEVER_SEND_ANTHROPIC_PRIVATE_37b1")
        .expect("store private-origin sentinel");
    let rejected = AnthropicProvider::new_subscription(
        private_vault
            .resolve(&private_alias)
            .expect("resolve private-origin sentinel"),
        "claude-audit",
        "http://169.254.169.254",
    )
    .expect_err("private fixed base must be rejected");
    assert_eq!(rejected.kind, ProviderErrorKind::InvalidRequest);

    for rebound_address in [
        SocketAddr::from(([127, 0, 0, 1], 443)),
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 443)),
        "[::ffff:127.0.0.1]:443"
            .parse()
            .expect("IPv4-mapped loopback address"),
        "100.100.100.200:443"
            .parse()
            .expect("RFC 6598 metadata address"),
        SocketAddr::from(([169, 254, 169, 254], 443)),
    ] {
        let rebound_vault = MemoryVault::new();
        let rebound_alias = CredentialAlias::new("anthropic-rebound-origin-audit");
        rebound_vault
            .put(&rebound_alias, b"NEVER_SEND_ANTHROPIC_REBOUND_c94d")
            .expect("store rebound sentinel");
        let rebound = AnthropicProvider::new_subscription_with_dns_resolver(
            rebound_vault
                .resolve(&rebound_alias)
                .expect("resolve rebound sentinel"),
            "claude-audit",
            ANTHROPIC_OAUTH_BASE_URL,
            Arc::new(StubFixedResolver {
                address: rebound_address,
            }),
        )
        .expect("construct fixed-host rebound audit");
        let rebound_error = rebound
            .request(&serde_json::json!({"model":"claude-audit"}))
            .await
            .expect_err("loopback/private DNS answer must fail before bearer construction");
        assert_eq!(rebound_error.kind, ProviderErrorKind::InvalidRequest);
    }
}

fn payload_provider(oauth: bool) -> AnthropicProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-payload-audit");
    vault
        .put(&alias, b"anthropic-payload-audit-sentinel")
        .expect("store payload audit secret");
    let credential = vault.resolve(&alias).expect("resolve payload audit secret");
    if oauth {
        AnthropicProvider::new_subscription(credential, "claude-audit", ANTHROPIC_OAUTH_BASE_URL)
            .expect("Anthropic subscription provider")
    } else {
        AnthropicProvider::new(credential, "claude-audit").expect("Anthropic key provider")
    }
}

fn payload_request(system_prompt: Option<&str>) -> TurnRequest {
    TurnRequest {
        messages: vec![Message::user_text("Reply with exactly: payload-audit")],
        model: "claude-audit".into(),
        max_tokens: 30_000,
        system_prompt: system_prompt.map(str::to_owned),
        tools: vec![
            ToolDefinition {
                name: "fs_read".into(),
                description: "Read a UTF-8 file".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                }),
            },
            ToolDefinition {
                name: "fs_search".into(),
                description: "Search UTF-8 files".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "anyOf": [{"required": ["pattern"]}, {"required": ["query"]}],
                    "oneOf": [{"required": ["pattern"]}],
                    "allOf": [{"type": "object"}],
                    "properties": {
                        "pattern": {"type": "string"},
                        "query": {"type": "string"},
                        "mode": {"anyOf": [{"const": "literal"}, {"const": "simple"}]},
                    },
                }),
            },
        ],
        attachments: Vec::new(),
        cache_metadata: None,
    }
}

fn cache_metadata(provider: &str, stable_history_end: usize) -> PromptCacheMetadata {
    PromptCacheMetadata {
        stable_history_end,
        current_user_start: stable_history_end,
        latest_compaction_summary_end: Some(1),
        prefix_digests: PrefixDigests {
            system: "system-digest".into(),
            tools: "tool-digest".into(),
            immutable_history: "history-digest".into(),
            model: "model-digest".into(),
            auth_mode: "auth-digest".into(),
            reasoning_settings: "reasoning-digest".into(),
        },
        cache_epoch: "epoch-a".into(),
        compaction_epoch: "compaction-a".into(),
        provider: provider.into(),
        session_scope: "session-a".into(),
        account_scope: Some("account-a".into()),
        stable_prefix_tokens: 8_192,
        expected_later_reads: 2,
        reuse_gap_ms: Some(30_000),
    }
}

fn cache_control_request() -> TurnRequest {
    TurnRequest {
        messages: vec![
            Message::user_text("Compacted history summary"),
            Message::user_text("prior question"),
            Message::assistant(vec![Block::Text {
                text: "prior answer".into(),
            }]),
            Message::user_text("current question"),
        ],
        model: "claude-audit".into(),
        max_tokens: 128,
        system_prompt: Some("Haider system".into()),
        tools: vec![ToolDefinition {
            name: "fs_read".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }],
        attachments: Vec::new(),
        cache_metadata: Some(cache_metadata("anthropic-oauth", 3)),
    }
}

fn strip_cache_control(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => values.iter_mut().for_each(strip_cache_control),
        serde_json::Value::Object(values) => {
            values.remove("cache_control");
            values.values_mut().for_each(strip_cache_control);
        }
        _ => {}
    }
}

/// CM2b — all four explicit Anthropic anchors are placed without moving or
/// decorating the OAuth identity block. The checked-in fixture is the exact
/// request body that crosses the adapter boundary.
///
/// MUTATION CHECK (executed): move any anchor by one message, annotate the
/// identity block, or drop tool/system/summary/history caching; the golden and
/// the explicit four-count assertion fail.
#[test]
fn cm2b_anthropic_four_breakpoints_oauth_identity_first_golden() {
    let payload = payload_provider(true)
        .with_prompt_caching_verified(true)
        .request_payload(&cache_control_request())
        .expect("cache-controlled OAuth payload");
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/anthropic/cache_control_request.json"
    ))
    .expect("Anthropic cache-control golden");
    assert_eq!(payload, expected);
    assert_eq!(
        payload["system"][0],
        serde_json::json!({"type": "text", "text": ANTHROPIC_OAUTH_SYSTEM_IDENTITY})
    );
    let cache_controls = payload.to_string().match_indices("cache_control").count();
    assert_eq!(
        cache_controls, 4,
        "exactly four explicit anchors: {payload}"
    );
}

/// CM2c — the longer TTL requires both a gap beyond five minutes and at
/// least two later reads; every uncertain/short/single-read case stays 5m.
///
/// MUTATION CHECK (executed): change `>` to `>=` or remove the read-count
/// conjunct; the boundary and one-read assertions fail.
#[test]
fn cm2c_anthropic_cache_ttl_requires_long_gap_and_two_reads() {
    assert_eq!(
        select_anthropic_cache_ttl(Some(300_001), 2),
        AnthropicCacheTtl::OneHour
    );
    for (gap, reads) in [
        (None, 2),
        (Some(300_000), 2),
        (Some(300_001), 1),
        (Some(10_000), 99),
    ] {
        assert_eq!(
            select_anthropic_cache_ttl(gap, reads),
            AnthropicCacheTtl::FiveMinutes
        );
    }
}

/// CM2f — consumer OAuth is deliberately unverified in production. Merely
/// attaching provider-neutral metadata must keep its CM1 full-history bytes.
#[test]
fn cm2f_unverified_anthropic_oauth_is_byte_exact_full_history() {
    let request = cache_control_request();
    let mut baseline = request.clone();
    baseline.cache_metadata = None;
    let provider = payload_provider(true);
    assert_eq!(
        provider
            .request_payload(&request)
            .expect("fallback payload"),
        provider.request_payload(&baseline).expect("CM1 payload")
    );

    let verified = payload_provider(true).with_prompt_caching_verified(true);
    let mut mismatched = request.clone();
    mismatched
        .cache_metadata
        .as_mut()
        .expect("metadata")
        .provider = "anthropic".into();
    assert_eq!(
        verified
            .request_payload(&mismatched)
            .expect("mismatch fallback"),
        verified.request_payload(&baseline).expect("CM1 payload")
    );

    let mut malformed = request;
    let metadata = malformed.cache_metadata.as_mut().expect("metadata");
    metadata.stable_history_end = 3;
    metadata.current_user_start = 2;
    assert_eq!(
        verified
            .request_payload(&malformed)
            .expect("boundary fallback"),
        verified.request_payload(&baseline).expect("CM1 payload")
    );
}

/// CM2g — cache metadata changes annotations only. After deleting those
/// ephemeral keys, the exact system, tools, message roles, text, and ordering
/// are identical to the unannotated request.
///
/// MUTATION CHECK (executed): truncate/reorder the stable messages while
/// annotating; stripping cache keys no longer recovers the baseline.
#[test]
fn cm2g_anthropic_annotations_do_not_change_model_visible_content() {
    let request = cache_control_request();
    let mut annotated = payload_provider(true)
        .with_prompt_caching_verified(true)
        .request_payload(&request)
        .expect("annotated payload");
    strip_cache_control(&mut annotated);
    let baseline = payload_provider(true)
        .request_payload(&request)
        .expect("unannotated payload");
    assert_eq!(annotated, baseline);
}

#[test]
fn cm2g_anthropic_api_key_system_text_and_signed_opaque_are_unchanged() {
    let provider = payload_provider(false);
    let mut request = cache_control_request();
    request.cache_metadata.as_mut().expect("metadata").provider = "anthropic".into();
    let signed = serde_json::json!({
        "type": "thinking",
        "thinking": "provider reasoning",
        "signature": "signed-provider-bytes"
    });
    request.messages[2].blocks.push(Block::ProviderOpaque {
        provider: "anthropic".into(),
        data: signed.clone(),
    });
    let annotated = provider
        .request_payload(&request)
        .expect("annotated API-key payload");
    let mut baseline_request = request;
    baseline_request.cache_metadata = None;
    let baseline = provider
        .request_payload(&baseline_request)
        .expect("baseline API-key payload");

    assert_eq!(annotated["system"][0]["text"], baseline["system"]);
    let mut stripped = annotated.clone();
    strip_cache_control(&mut stripped);
    assert_eq!(stripped["messages"], baseline["messages"]);
    assert_eq!(annotated["messages"][2]["content"][1], signed);
    assert!(
        annotated["messages"][2]["content"][1]
            .get("cache_control")
            .is_none(),
        "signed terminal blocks are never decorated"
    );
}

/// CM2a (final wire) — append-only history does not perturb Haider-owned
/// system/tool bytes or their canonical digests. A real owned-input mutation
/// changes the corresponding digest.
#[test]
fn cm2a_anthropic_final_wire_system_and_tool_digests_are_stable() {
    let provider = payload_provider(true).with_prompt_caching_verified(true);
    let first = cache_control_request();
    let first_digests = provider
        .rendered_cache_prefix_digests(&first)
        .expect("first rendered digests");
    let mut second = first.clone();
    second.messages.push(Message::assistant(vec![Block::Text {
        text: "current answer".into(),
    }]));
    second.messages.push(Message::user_text("next question"));
    let metadata = second.cache_metadata.as_mut().expect("cache metadata");
    metadata.stable_history_end = 5;
    metadata.current_user_start = 5;
    let second_digests = provider
        .rendered_cache_prefix_digests(&second)
        .expect("second rendered digests");
    assert_eq!(first_digests.system, second_digests.system);
    assert_eq!(first_digests.tools, second_digests.tools);

    let mut mutated = second;
    mutated.tools[0].description.push_str(" mutated");
    mutated.system_prompt = Some("mutated system".into());
    let mutated_digests = provider
        .rendered_cache_prefix_digests(&mutated)
        .expect("mutated rendered digests");
    assert_ne!(first_digests.system, mutated_digests.system);
    assert_ne!(first_digests.tools, mutated_digests.tools);
}

/// MUTATION CHECK: change or drop the identity text, merge it into the turn's
/// own system block, reorder the blocks, omit `system` on a promptless OAuth
/// turn, or leak the identity into api-key mode. Each mutation fails a named
/// assertion below. Live law: Anthropic rejects OAuth-subscription bodies
/// whose `system` does not open with the exact Claude Code identity block
/// (captured 2026-08-05: schema-valid identity-free OAuth turns were refused
/// with generic-"Error" responses on every attempt).
#[test]
fn oauth_payload_opens_system_with_claude_code_identity_block() {
    let oauth = payload_provider(true);
    let request = payload_request(Some("haider-system-v2\nYou are Haider Code."));
    let payload = oauth.request_payload(&request).expect("OAuth payload");

    let system = payload["system"]
        .as_array()
        .expect("OAuth system is an array of blocks");
    assert_eq!(system.len(), 2, "identity block plus the turn's own prompt");
    assert_eq!(
        system[0],
        serde_json::json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
        }),
        "first system block is exactly the Claude Code identity line"
    );
    assert_eq!(
        system[1],
        serde_json::json!({
            "type": "text",
            "text": "haider-system-v2\nYou are Haider Code.",
        }),
        "the turn's real system prompt rides as its own second block"
    );

    // A promptless OAuth turn still carries the identity block: omitting
    // `system` entirely is rejected by the same server-side validation.
    let bare = oauth
        .request_payload(&payload_request(None))
        .expect("promptless OAuth payload");
    assert_eq!(
        bare["system"],
        serde_json::json!([{
            "type": "text",
            "text": ANTHROPIC_OAUTH_SYSTEM_IDENTITY,
        }]),
        "promptless OAuth turns send exactly the identity block"
    );
}

/// MUTATION CHECK: prepend the identity to api-key bodies, turn the api-key
/// system into an array, or let the two modes drift anywhere outside
/// `system`. Each mutation fails a named assertion below.
#[test]
fn api_key_payload_keeps_plain_system_and_matches_oauth_outside_system() {
    let api_key = payload_provider(false);
    let oauth = payload_provider(true);
    let request = payload_request(Some("haider-system-v2\nYou are Haider Code."));

    let key_payload = api_key.request_payload(&request).expect("api-key payload");
    assert_eq!(
        key_payload["system"],
        serde_json::Value::String("haider-system-v2\nYou are Haider Code.".into()),
        "api-key mode sends the turn's system prompt as a plain string"
    );
    assert!(
        !key_payload.to_string().contains("Claude Code"),
        "api-key mode never carries the OAuth identity line"
    );

    let bare = api_key
        .request_payload(&payload_request(None))
        .expect("promptless api-key payload");
    assert!(
        bare.get("system").is_none(),
        "promptless api-key turns omit `system` entirely"
    );

    // Golden cross-mode law: the two bodies differ in `system` and nowhere else.
    let mut key_rest = key_payload;
    let mut oauth_rest = oauth.request_payload(&request).expect("OAuth payload");
    key_rest.as_object_mut().expect("object").remove("system");
    oauth_rest.as_object_mut().expect("object").remove("system");
    assert_eq!(
        key_rest, oauth_rest,
        "auth modes agree on every field except `system`"
    );
}

/// MUTATION CHECK: keep any of `oneOf`/`allOf`/`anyOf` at the top level of a
/// tool schema, or strip the nested `anyOf` too, and a named assertion below
/// fails. Live law: Anthropic's Messages API rejects top-level combinators in
/// custom tool schemas for both auth modes (captured 2026-08-05: HTTP 400
/// "tools.3.custom.input_schema: input_schema does not support oneOf, allOf,
/// or anyOf at the top level").
#[test]
fn tool_schemas_drop_top_level_combinators_for_both_auth_modes() {
    for oauth in [false, true] {
        let provider = payload_provider(oauth);
        let payload = provider
            .request_payload(&payload_request(Some("prompt")))
            .expect("payload");
        let tools = payload["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2, "both tools survive schema shaping");
        assert_eq!(
            tools[0]["input_schema"],
            payload_request(None).tools[0].input_schema,
            "combinator-free schemas pass through byte-identical (oauth={oauth})"
        );
        let search_schema = &tools[1]["input_schema"];
        for banned in ["oneOf", "allOf", "anyOf"] {
            assert!(
                search_schema.get(banned).is_none(),
                "top-level `{banned}` is dropped from the Anthropic body (oauth={oauth})"
            );
        }
        assert_eq!(
            search_schema["properties"]["mode"],
            serde_json::json!({"anyOf": [{"const": "literal"}, {"const": "simple"}]}),
            "nested combinators are preserved (oauth={oauth})"
        );
        assert_eq!(
            search_schema["additionalProperties"],
            serde_json::json!(false),
            "unrelated schema keys are untouched (oauth={oauth})"
        );
    }
}

impl HangingFixture {
    fn new() -> Self {
        let fixture = include_bytes!("../tests/fixtures/anthropic/hanging_mid_turn.sse").as_slice();
        assert!(fixture.ends_with(b"\n"));
        assert!(!fixture.ends_with(b"\n\n"));
        let mut first_chunk = fixture.to_vec();
        first_chunk.push(b'\n');
        Self {
            first_chunk: Some(first_chunk),
        }
    }
}

impl SseChunkSource for HangingFixture {
    async fn next_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError> {
        if let Some(chunk) = self.first_chunk.take() {
            return Ok(Some(chunk));
        }
        future::pending().await
    }
}

#[tokio::test]
async fn hanging_mid_turn_fixture_times_out_only_the_idle_chunk_await() {
    tokio::time::pause();
    let (sender, mut receiver) = mpsc::channel(4);
    let stream_task = tokio::spawn(stream_sse_source(
        HangingFixture::new(),
        None,
        sender,
        Duration::from_secs(90),
    ));

    assert_eq!(
        receiver.recv().await,
        Some(Ok(StreamEvent::TextDelta {
            text: "partial".into(),
        }))
    );

    tokio::time::advance(Duration::from_secs(89)).await;
    tokio::task::yield_now().await;
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

    tokio::time::advance(Duration::from_secs(1)).await;
    let error = receiver
        .recv()
        .await
        .expect("idle deadline emits one item")
        .expect_err("idle deadline is a typed error");
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert!(error.retryable);
    assert!(error.message.contains("90 seconds"));
    assert!(receiver.recv().await.is_none(), "failure is surfaced once");
    stream_task.await.expect("stream task exits after timeout");
}

/// LAW (LE3, anthropic half): the session effort rides `output_config.effort`
/// VERBATIM on BOTH auth modes; the body NEVER carries a `thinking` field
/// (`thinking.budget_tokens` 400s on 4.7+ and every 5-family model), and —
/// pinning brief decision 10 — never `temperature`/`top_p`/`top_k`. With no
/// effort the payload keeps its exact pre-G3 top-level key set.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): route the effort
/// through `thinking: {"budget_tokens": ...}` instead of `output_config`.
/// Expected runtime failure: the no-thinking-field and output_config
/// assertions below.
#[test]
fn effort_rides_output_config_and_never_thinking_or_sampling_params() {
    for oauth in [false, true] {
        let provider = payload_provider(oauth).with_effort(Some("xhigh".into()));
        let payload = provider
            .request_payload(&payload_request(Some("system prompt")))
            .expect("payload with effort");
        assert_eq!(
            payload["output_config"],
            serde_json::json!({"effort": "xhigh"}),
            "effort rides output_config (oauth={oauth}): {payload}"
        );
        let object = payload.as_object().expect("payload object");
        for forbidden in ["thinking", "temperature", "top_p", "top_k", "speed"] {
            assert!(
                !object.contains_key(forbidden),
                "`{forbidden}` must not ride an effort-only payload (oauth={oauth}): {payload}"
            );
        }

        // Without an effort the pre-G3 body shape is byte-stable: the exact
        // top-level key set, no output_config.
        let plain = payload_provider(oauth)
            .request_payload(&payload_request(Some("system prompt")))
            .expect("payload without effort");
        let mut keys: Vec<&str> = plain
            .as_object()
            .expect("plain object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "max_tokens",
                "messages",
                "model",
                "stream",
                "system",
                "tools"
            ],
            "the effortless payload keeps the pre-G3 key set (oauth={oauth})"
        );
    }
}

/// LAW (LE4, wire half): fast mode is `speed: "fast"` in the body PLUS the
/// `fast-mode-2026-02-01` beta header — comma-joined AFTER the OAuth beta on
/// subscription requests, alone on api-key requests — and fast OFF keeps the
/// exact pre-G3 header value.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): replace the OAuth
/// comma-join with the fast beta ALONE. Expected runtime failure: the OAuth
/// header assertion below (and live, the subscription identity check 400s).
#[tokio::test]
async fn fast_mode_sets_speed_body_and_comma_joined_beta_header() {
    // Body: both auth modes carry the top-level speed field.
    for oauth in [false, true] {
        let payload = payload_provider(oauth)
            .with_fast(true)
            .request_payload(&payload_request(Some("system prompt")))
            .expect("fast payload");
        assert_eq!(
            payload["speed"], "fast",
            "fast rides the body (oauth={oauth}): {payload}"
        );
    }

    // Header, api-key mode: the fast beta alone.
    let api_key = payload_provider(false).with_fast(true);
    let request = api_key
        .request(&serde_json::json!({"model": "claude-audit"}))
        .await
        .expect("api-key fast request");
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .expect("fast beta header"),
        ANTHROPIC_FAST_BETA_VALUE
    );

    // Header, api-key mode, fast OFF: no beta header at all (pre-G3 shape).
    let request = payload_provider(false)
        .request(&serde_json::json!({"model": "claude-audit"}))
        .await
        .expect("api-key standard request");
    assert!(request.headers().get(ANTHROPIC_OAUTH_BETA_HEADER).is_none());

    // Header, OAuth mode: ONE comma-joined value, subscription beta FIRST.
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-fast-header-audit");
    vault
        .put(&alias, b"ANTHROPIC_FAST_SENTINEL_77aa")
        .expect("store OAuth access");
    let oauth = AnthropicProvider::new_subscription_with_dns_resolver(
        vault.resolve(&alias).expect("resolve OAuth access"),
        "claude-audit",
        ANTHROPIC_OAUTH_BASE_URL,
        Arc::new(StubFixedResolver {
            address: SocketAddr::from(([93, 184, 216, 34], 443)),
        }),
    )
    .expect("Anthropic subscription provider")
    .with_fast(true);
    let request = oauth
        .request(&serde_json::json!({"model": "claude-audit"}))
        .await
        .expect("oauth fast request");
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .expect("oauth+fast beta header"),
        "oauth-2025-04-20,fast-mode-2026-02-01"
    );
    assert_eq!(
        request
            .headers()
            .get_all(ANTHROPIC_OAUTH_BETA_HEADER)
            .iter()
            .count(),
        1,
        "the betas comma-join into ONE header value"
    );
}

// ───────────────────────── G4b enterprise endpoints ─────────────────────────

fn secret_credential(alias: &str, secret: &[u8]) -> haider_accounts::SecretHandle {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new(alias);
    vault.put(&alias, secret).expect("store test secret");
    vault.resolve(&alias).expect("resolve test secret")
}

fn one_line_turn(model: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![Message::user_text("ping")],
        model: model.into(),
        max_tokens: 16,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    }
}

/// CM1a — captured Anthropic usage keeps its separate read/write semantics,
/// including the provider's 5m/1h creation detail.
///
/// MUTATION CHECK (executed): map cache creation into reads, or omit the 1h
/// split; the unequal 100/70/30/10/20 assertions fail.
#[test]
fn cm1a_anthropic_separate_read_write_decode() {
    use haider_protocol::provider::{CacheStatAvailability, StreamEvent};

    let events = replay_anthropic_sse(include_bytes!(
        "../tests/fixtures/anthropic/cache_usage_split.sse"
    ));
    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
            _ => None,
        })
        .expect("captured usage update");
    let normalized = usage.normalized.as_ref().expect("normalized usage");
    assert_eq!(normalized.logical_input, 200);
    assert_eq!(normalized.uncached_input, 130);
    assert_eq!(normalized.cache_read_input, 70);
    assert_eq!(normalized.cache_write_input, 30);
    assert_eq!(normalized.cache_write_5m_input, 10);
    assert_eq!(normalized.cache_write_1h_input, 20);
    assert_eq!(normalized.cache_status, CacheStatAvailability::Present);
    assert_eq!(
        normalized.cache_write_ttl_status,
        CacheStatAvailability::Present
    );
}

/// LAW (LB1 — the mantle golden): the Bedrock adapter POSTs
/// `{base}/v1/messages` with the bearer riding `x-api-key`, the standard
/// `anthropic-version: 2023-06-01` header, NO Authorization header, the
/// `anthropic.`-prefixed model IN THE BODY, and decodes a scripted standard
/// SSE stream — the mantle wire is the first-party Messages wire verbatim.
///
/// MUTATION CHECK: swap the mantle URL template, move the credential to
/// Authorization, drop the version header, or drop body.model. Expected
/// RUNTIME failure: the named equalities below.
#[tokio::test]
async fn lb1_bedrock_mantle_golden_url_headers_body_and_sse() {
    let provider = AnthropicProvider::new_endpoint(
        secret_credential("bedrock-mantle-audit", b"BEDROCK_BEARER_SENTINEL_44aa"),
        "anthropic.claude-opus-5",
        "https://bedrock-mantle.us-east-1.api.aws/anthropic",
    )
    .expect("bedrock mantle adapter");
    let payload = provider
        .request_payload(&one_line_turn("anthropic.claude-opus-5"))
        .expect("mantle payload");
    assert_eq!(
        payload.get("model").and_then(serde_json::Value::as_str),
        Some("anthropic.claude-opus-5"),
        "the mantle model rides IN THE BODY"
    );
    let request = provider.request(&payload).await.expect("mantle request");
    assert_eq!(
        request.url().as_str(),
        "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages"
    );
    assert_eq!(
        request
            .headers()
            .get("x-api-key")
            .expect("x-api-key bearer"),
        "BEDROCK_BEARER_SENTINEL_44aa"
    );
    assert_eq!(
        request
            .headers()
            .get("anthropic-version")
            .expect("standard version header"),
        "2023-06-01"
    );
    assert!(
        !request.headers().contains_key(AUTHORIZATION),
        "mantle bearer must never ride Authorization"
    );
    assert_eq!(
        provider.credential_surface(),
        crate::ProviderCredentialSurface::ApiKey,
        "the mantle surface is the EXACT x-api-key reuse (decision 5)"
    );
    // Scripted standard SSE decodes through the unmodified decoder.
    let stream = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"mantle ok\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";
    let items = crate::replay_anthropic_sse(stream.as_bytes());
    assert!(
        items.iter().any(|item| matches!(
            item,
            Ok(StreamEvent::TextDelta { text }) if text == "mantle ok"
        )),
        "standard SSE text decodes"
    );
    assert!(
        items
            .iter()
            .any(|item| matches!(item, Ok(StreamEvent::Finish { .. }))),
        "standard SSE finish decodes"
    );
}

/// LAW (LB2 — endpoint pinning): `new_endpoint` accepts EXACTLY the mantle
/// URL shape and refuses everything else, so the bearer can never be aimed
/// at an arbitrary origin. Both directions: two valid regions construct;
/// the refusal matrix stays refused.
///
/// MUTATION CHECK: accept any https URL in
/// `validate_bedrock_mantle_base_url`. Expected RUNTIME failure: the
/// refusal matrix below constructs adapters.
#[test]
fn lb2_new_endpoint_refuses_non_mantle_url_shapes() {
    for accepted in [
        "https://bedrock-mantle.us-east-1.api.aws/anthropic",
        "https://bedrock-mantle.eu-central-1.api.aws/anthropic/",
    ] {
        AnthropicProvider::new_endpoint(
            secret_credential("bedrock-shape-audit", b"NEVER_SENT_SHAPE_AUDIT"),
            "anthropic.claude-opus-5",
            accepted,
        )
        .unwrap_or_else(|error| panic!("mantle shape `{accepted}` must construct: {error}"));
    }
    for refused in [
        "https://api.anthropic.com/v1/messages",
        "http://bedrock-mantle.us-east-1.api.aws/anthropic",
        "https://bedrock-mantle.us-east-1.api.aws.evil.example/anthropic",
        "https://bedrock-mantle.us-east-1.api.aws/anthropic/extra",
        "https://bedrock-mantle..api.aws/anthropic",
        "https://bedrock-mantle.Us-East-1.api.aws/anthropic",
        "https://bedrock-mantle.us.east/1.api.aws/anthropic",
        "https://bedrock-mantle.us-east-1.api.aws",
        "",
    ] {
        let error = AnthropicProvider::new_endpoint(
            secret_credential("bedrock-shape-audit", b"NEVER_SENT_SHAPE_AUDIT"),
            "anthropic.claude-opus-5",
            refused,
        )
        .err()
        .unwrap_or_else(|| panic!("non-mantle shape `{refused}` must be refused"));
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    }
}

/// LAW (LV1 — the vertex golden): the Vertex adapter POSTs
/// `{base}/{model}:streamRawPredict` (model IN THE URL), the body carries
/// `anthropic_version: "vertex-2023-10-16"` and NO `model` field, auth is a
/// plain `Authorization: Bearer`, and neither `x-api-key`, the OAuth beta,
/// nor the standard `anthropic-version` HEADER is sent.
///
/// MUTATION CHECK: keep `model` in the body, drop the `anthropic_version`
/// insert, or template the first-party URL. Expected RUNTIME failure: the
/// named equalities below.
#[tokio::test]
async fn lv1_vertex_golden_model_in_url_version_in_body_bearer_header() {
    let provider = AnthropicProvider::new_vertex(
        secret_credential("vertex-audit", b"VERTEX_GCP_TOKEN_SENTINEL_77cc"),
        "claude-sonnet-4-5@20250929",
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
    )
    .expect("vertex adapter");
    let payload = provider
        .request_payload(&one_line_turn("claude-sonnet-4-5@20250929"))
        .expect("vertex payload");
    assert!(
        payload.get("model").is_none(),
        "the vertex body must NOT carry a model field"
    );
    assert_eq!(
        payload
            .get("anthropic_version")
            .and_then(serde_json::Value::as_str),
        Some("vertex-2023-10-16"),
        "the vertex body versions through anthropic_version"
    );
    let request = provider.request(&payload).await.expect("vertex request");
    assert_eq!(
        request.url().as_str(),
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models/claude-sonnet-4-5@20250929:streamRawPredict",
        "the model rides IN THE URL"
    );
    assert_eq!(
        request.headers().get(AUTHORIZATION).expect("GCP bearer"),
        "Bearer VERTEX_GCP_TOKEN_SENTINEL_77cc"
    );
    assert!(!request.headers().contains_key("x-api-key"));
    assert!(!request.headers().contains_key(ANTHROPIC_OAUTH_BETA_HEADER));
    assert!(
        !request.headers().contains_key("anthropic-version"),
        "vertex versions through the BODY, never the header"
    );
    assert_eq!(
        provider.credential_surface(),
        crate::ProviderCredentialSurface::CloudBearer,
        "the vertex surface is CloudBearer (decision 5)"
    );
}

/// LAW (LV1, shape half): the Vertex base-URL validator accepts the global
/// and matching-regional templates and refuses host/path disagreement,
/// non-Google hosts, and http — both directions.
#[test]
fn vertex_base_url_shape_is_pinned_global_or_matching_regional() {
    use crate::anthropic::{validate_vertex_models_base_url, vertex_models_base_url};
    for accepted in [
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-ai/locations/us-east5/publishers/anthropic/models",
    ] {
        validate_vertex_models_base_url(accepted)
            .unwrap_or_else(|error| panic!("vertex shape `{accepted}` must pass: {error}"));
    }
    for refused in [
        "http://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/us-east5/publishers/anthropic/models",
        "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-ai/locations/eu-west4/publishers/anthropic/models",
        "https://evilaiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://aiplatform.googleapis.example/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://aiplatform.googleapis.com/v1/projects/acme.ai/locations/global/publishers/anthropic/models",
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/google/models",
        "",
    ] {
        assert!(
            validate_vertex_models_base_url(refused).is_err(),
            "vertex shape `{refused}` must be refused"
        );
    }
    // The card-side builder routes through the SAME validator.
    assert_eq!(
        vertex_models_base_url("acme-ai", "global").expect("global build"),
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models"
    );
    assert_eq!(
        vertex_models_base_url("acme-ai", "us-east5").expect("regional build"),
        "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-ai/locations/us-east5/publishers/anthropic/models"
    );
    assert!(vertex_models_base_url("", "global").is_err());
}
