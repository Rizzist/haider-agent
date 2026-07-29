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
    AnthropicProvider, SseChunkSource, stream_sse_source,
};
use crate::origin::FixedDnsResolver;
use crate::{ProviderError, ProviderErrorKind};

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
