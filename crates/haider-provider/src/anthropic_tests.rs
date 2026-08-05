#![allow(clippy::expect_used)]

use std::future;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::provider::StreamEvent;
use reqwest::header::AUTHORIZATION;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::anthropic::{
    ANTHROPIC_OAUTH_BASE_URL, ANTHROPIC_OAUTH_BETA_HEADER, ANTHROPIC_OAUTH_BETA_VALUE,
    ANTHROPIC_OAUTH_SYSTEM_IDENTITY, AnthropicProvider, SseChunkSource, stream_sse_source,
};
use crate::origin::FixedDnsResolver;
use crate::{Message, ProviderError, ProviderErrorKind, ToolDefinition, TurnRequest};

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
    }
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
