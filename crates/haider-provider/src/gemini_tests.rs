#![allow(clippy::expect_used)]

use std::future;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::provider::{FinishReason, StreamEvent};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::gemini::{
    GeminiDecoder, GeminiProvider, GeminiRetryPolicy, GeminiSseChunkSource,
    parse_protobuf_duration_ms, stream_sse_source,
};
use crate::origin::FixedDnsResolver;
use crate::{ProviderError, ProviderErrorKind};

struct StubFixedResolver {
    address: SocketAddr,
}

#[async_trait]
impl FixedDnsResolver for StubFixedResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![self.address])
    }
}

fn provider_with_resolver(address: SocketAddr) -> GeminiProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("gemini-origin-audit");
    vault
        .put(&alias, b"GEMINI_API_KEY_SENTINEL_91a7")
        .expect("stores audit key");
    GeminiProvider::new_with_dns_resolver(
        vault.resolve(&alias).expect("resolves audit key"),
        "gemini-2.5-flash",
        Arc::new(StubFixedResolver { address }),
    )
    .expect("constructs Gemini provider")
}

#[test]
fn constructor_transport_config_disables_retries_and_pins_all_timeouts() {
    let config = GeminiProvider::transport_config();
    assert_eq!(config.retry_policy, GeminiRetryPolicy::Never);
    assert_eq!(config.connect_timeout, Duration::from_secs(10));
    assert_eq!(config.response_open_timeout, Duration::from_secs(30));
    assert_eq!(config.chunk_idle_timeout, Duration::from_secs(90));
}

#[test]
fn gemini_credential_client_ignores_inherited_proxy_environment() {
    const CHILD_MARKER: &str = "HAIDER_GEMINI_PROXY_PIN_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let provider = provider_with_resolver(SocketAddr::from(([93, 184, 216, 34], 443)));
        assert!(
            !provider.client_debug().contains("proxies"),
            "Gemini credential-bearing client retained inherited proxy configuration"
        );
        return;
    }

    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .arg("gemini_credential_client_ignores_inherited_proxy_environment")
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
        .expect("runs isolated Gemini proxy child");
    assert!(
        output.status.success(),
        "Gemini proxy child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// MUTATION CHECK: replace the API-key header with bearer auth, omit the
/// fixed `alt=sse` query, build the request before origin validation, or
/// disconnect reqwest from the pinned resolver. Each mutation changes an
/// assertion before a credential can reach a network peer.
#[tokio::test]
async fn x_goog_api_key_is_sensitive_and_request_consumes_fixed_origin_guard() {
    let provider = provider_with_resolver(SocketAddr::from(([93, 184, 216, 34], 443)));
    let request = provider
        .request(&serde_json::json!({"contents": []}))
        .await
        .expect("builds pinned request");
    assert_eq!(
        request.url().as_str(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    let api_key = request
        .headers()
        .get("x-goog-api-key")
        .expect("Gemini API-key header");
    assert_eq!(api_key, "GEMINI_API_KEY_SENTINEL_91a7");
    assert!(api_key.is_sensitive());
    assert!(
        !request
            .headers()
            .contains_key(reqwest::header::AUTHORIZATION)
    );

    provider.stall_fixed_connection_resolution();
    let execution = provider.execute_request_for_test(request);
    tokio::pin!(execution);
    let resolution_observed = async {
        while provider.fixed_connection_resolution_count() == 0 {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        result = &mut execution => panic!("fixed resolver did not stall request: {result:?}"),
        observed = tokio::time::timeout(Duration::from_secs(1), resolution_observed) => {
            observed.expect("reqwest consumes pinned Gemini resolver");
        }
    }
    assert_eq!(provider.fixed_connection_resolution_count(), 1);
}

#[tokio::test]
async fn private_or_special_dns_answers_fail_before_api_key_request_building() {
    for address in [
        SocketAddr::from(([127, 0, 0, 1], 443)),
        SocketAddr::from(([169, 254, 169, 254], 443)),
        "100.100.100.200:443".parse().expect("metadata address"),
        "[::ffff:127.0.0.1]:443".parse().expect("mapped loopback"),
    ] {
        let provider = provider_with_resolver(address);
        let error = provider
            .request(&serde_json::json!({"contents": []}))
            .await
            .expect_err("private resolution is rejected");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(!error.message.contains("GEMINI_API_KEY_SENTINEL_91a7"));
    }
}

#[test]
fn synthesized_call_ids_are_deterministic_and_accept_a_history_offset() {
    let bytes = b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"weather\",\"args\":{\"city\":\"Tehran\"}}}]},\"finishReason\":\"STOP\"}]}\n\n";
    let decode = |base| {
        let mut decoder = GeminiDecoder::new(None, base);
        let mut items = decoder.push(bytes);
        items.extend(decoder.finish());
        items
    };
    assert_eq!(decode(0), decode(0));
    assert!(matches!(
        &decode(0)[0],
        Ok(StreamEvent::ToolCallStart { call_id, .. })
            if call_id == "gemini-call-0000000000000000"
    ));
    assert!(matches!(
        &decode(7)[0],
        Ok(StreamEvent::ToolCallStart { call_id, .. })
            if call_id == "gemini-call-0000000000000007"
    ));
    assert!(matches!(
        decode(7).last(),
        Some(Ok(StreamEvent::Finish {
            reason: FinishReason::ToolUse
        }))
    ));
}

#[test]
fn retry_info_protobuf_durations_are_millisecond_exact_and_bounded() {
    assert_eq!(parse_protobuf_duration_ms("3s"), Some(3_000));
    assert_eq!(parse_protobuf_duration_ms("1.25s"), Some(1_250));
    assert_eq!(parse_protobuf_duration_ms("0.0075s"), Some(7));
    assert_eq!(parse_protobuf_duration_ms("bad"), None);
    assert_eq!(parse_protobuf_duration_ms("1.-2s"), None);
}

struct HangingFixture {
    first_chunk: Option<Vec<u8>>,
}

impl GeminiSseChunkSource for HangingFixture {
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
        HangingFixture {
            first_chunk: Some(
                b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n"
                    .to_vec(),
            ),
        },
        None,
        0,
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
        .expect("idle timeout item")
        .expect_err("idle timeout is typed");
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert!(error.message.contains("90 seconds"));
    assert!(receiver.recv().await.is_none());
    stream_task.await.expect("stream task exits");
}
