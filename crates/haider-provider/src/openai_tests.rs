#![allow(clippy::expect_used)]

use super::*;
use std::collections::VecDeque;
use std::future;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::{CatalogSource, Message, PromptCacheMetadata, ToolDefinition, UserCommandRecord};
use haider_accounts::{MemoryVault, Vault};
use haider_protocol::ids::ArtifactRef;
use haider_protocol::item::ToolStatus;
use haider_protocol::provider::PrefixDigests;
use haider_protocol::tool::ImageBlockRef;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::error::TryRecvError;

#[test]
fn shared_provider_builder_pins_idle_h2_and_tcp_keep_alive() {
    assert_eq!(
        crate::PROVIDER_KEEP_ALIVE,
        crate::ProviderKeepAliveConfig {
            http2_interval: std::time::Duration::from_secs(30),
            http2_while_idle: true,
            tcp_interval: std::time::Duration::from_secs(30),
        }
    );
    crate::provider_http_client_builder()
        .build()
        .expect("keep-alive provider client builder");
}

fn serialized_json_body(payload: serde_json::Value) -> Vec<u8> {
    crate::serialize_json_body(payload).expect("serialize provider request body")
}

struct HangingFixture {
    first_chunk: Option<Vec<u8>>,
    dropped: Arc<AtomicBool>,
}

impl HangingFixture {
    fn new(dropped: Arc<AtomicBool>) -> Self {
        let fixture = include_bytes!("../tests/fixtures/openai/hanging_mid_turn.sse").as_slice();
        assert!(fixture.ends_with(b"\n"));
        assert!(!fixture.ends_with(b"\n\n"));
        let mut first_chunk = fixture.to_vec();
        first_chunk.push(b'\n');
        Self {
            first_chunk: Some(first_chunk),
            dropped,
        }
    }
}

impl Drop for HangingFixture {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
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

struct BodyFixture {
    declared_length: Option<u64>,
    chunks: VecDeque<Vec<u8>>,
}

struct StubDnsResolver {
    answers: Mutex<VecDeque<Vec<SocketAddr>>>,
    calls: AtomicUsize,
}

impl StubDnsResolver {
    fn new(answers: impl IntoIterator<Item = Vec<SocketAddr>>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl FixedDnsResolver for StubDnsResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .expect("fixed resolver answer lock")
            .pop_front()
            .ok_or_else(|| std::io::Error::other("fixed resolver had no answer"))
    }
}

impl BodyChunkSource for BodyFixture {
    fn content_length_hint(&self) -> Option<u64> {
        self.declared_length
    }

    async fn next_body_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError> {
        Ok(self.chunks.pop_front())
    }
}

#[tokio::test]
async fn hostile_probe_and_http_error_body_sources_are_bounded() {
    let probe_error = read_body_source_bounded(
        BodyFixture {
            declared_length: None,
            chunks: VecDeque::from([vec![b'x'; MODELS_BODY_LIMIT], vec![b'x'; 1]]),
        },
        MODELS_BODY_LIMIT,
        "OpenAI-compatible /v1/models",
    )
    .await
    .expect_err("streaming models body exceeds cap");
    assert_eq!(probe_error.kind, ProviderErrorKind::MalformedFrame);
    assert!(probe_error.message.contains("1048576-byte limit"));

    let early_error = read_body_source_bounded(
        BodyFixture {
            declared_length: Some((ERROR_BODY_LIMIT + 1) as u64),
            chunks: VecDeque::new(),
        },
        ERROR_BODY_LIMIT,
        "OpenAI HTTP error",
    )
    .await
    .expect_err("declared error body exceeds cap");
    let classified = classify_http_body_read_error(503, None, early_error);
    assert_eq!(classified.kind, ProviderErrorKind::Overloaded);
    assert!(classified.retryable);
    assert!(classified.message.contains("65536-byte limit"));
}

/// MUTATION CHECK: discard the extracted provider message, or restrict HTTP
/// detail extraction to the typed OpenAI envelope. The exact parsed and raw
/// explanations below disappear from the durable presentation.
#[test]
fn openai_http_errors_preserve_safe_provider_explanations() {
    let parsed = replay_openai_http_error(
        400,
        None,
        br#"{"error":{"type":"invalid_request_error","message":"Unsupported parameter: service_tier"}}"#,
    );
    assert_eq!(parsed.kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(
        parsed.presentation.detail,
        "Unsupported parameter: service_tier"
    );

    let plain = replay_openai_http_error(
        400,
        None,
        b"The service is overloaded. Please try again later.",
    );
    assert_eq!(plain.kind, ProviderErrorKind::Overloaded);
    assert!(plain.retryable);
    assert_eq!(
        plain.presentation.detail,
        "The service is overloaded. Please try again later."
    );

    let redacted = replay_openai_http_error(
        400,
        None,
        br#"{"error":{"message":"Credential Bearer sk-provider-secret-value was rejected"}}"#,
    );
    assert_eq!(
        redacted.presentation.detail,
        "Credential Bearer [REDACTED] was rejected"
    );
    assert!(
        !redacted
            .presentation
            .detail
            .contains("sk-provider-secret-value")
    );

    // MUTATION CHECK: clear `redact_next` after consuming `Bearer`. The
    // opaque credential following `Authorization: Bearer` becomes durable.
    let redacted = replay_openai_http_error(
        400,
        None,
        br#"{"error":{"message":"Credential Authorization: Bearer opaque-provider-token-value was rejected"}}"#,
    );
    assert_eq!(
        redacted.presentation.detail,
        "Credential Authorization: [REDACTED] [REDACTED] was rejected"
    );
    assert!(
        !redacted
            .presentation
            .detail
            .contains("opaque-provider-token-value")
    );

    // MUTATION CHECK: remove prose-based authentication classification. The
    // backend has historically returned this explanation under HTTP 400.
    let invalidated = replay_openai_http_error(
        400,
        None,
        br#"{"error":{"code":"invalid_request_error","message":"Your authentication token has been invalidated. Please sign in again."}}"#,
    );
    assert_eq!(invalidated.kind, ProviderErrorKind::Authentication);
    assert!(!invalidated.retryable);
    assert_eq!(
        invalidated.presentation.detail,
        "Your authentication token has been invalidated. Please sign in again."
    );
}

/// MUTATION CHECK: return the kind-only stream error or require JSON data for
/// `event:error`. The provider's explanation is lost in one of these cases.
#[test]
fn openai_stream_errors_preserve_enveloped_and_raw_explanations() {
    let enveloped = replay_openai_responses_sse(
        b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_request_error\",\"message\":\"Unknown field: metadata\"}}}\n\n",
    );
    let error = enveloped
        .into_iter()
        .next()
        .expect("one stream item")
        .expect_err("response.failed is an error");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(error.presentation.detail, "Unknown field: metadata");

    let raw = replay_openai_responses_sse(
        b"event: error\ndata: The service is overloaded. Please try again later.\n\n",
    );
    let error = raw
        .into_iter()
        .next()
        .expect("one stream item")
        .expect_err("raw error frame is an error");
    assert_eq!(error.kind, ProviderErrorKind::Overloaded);
    assert!(error.retryable);
    assert_eq!(
        error.presentation.detail,
        "The service is overloaded. Please try again later."
    );

    let compatible =
        replay_openai_chat_sse(b"event: error\ndata: The compatible upstream is overloaded.\n\n");
    let error = compatible
        .into_iter()
        .next()
        .expect("one compatible stream item")
        .expect_err("raw compatible error frame is an error");
    assert_eq!(error.kind, ProviderErrorKind::Overloaded);
    assert_eq!(
        error.presentation.detail,
        "The compatible upstream is overloaded."
    );

    let invalidated = replay_openai_responses_sse(
        b"event: error\ndata: Your authentication token has been invalidated. Please sign in again.\n\n",
    );
    let error = invalidated
        .into_iter()
        .next()
        .expect("one authentication stream item")
        .expect_err("invalidated token is an error");
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(!error.retryable);
    assert_eq!(
        error.presentation.detail,
        "Your authentication token has been invalidated. Please sign in again."
    );
}

#[tokio::test]
async fn hanging_openai_fixture_times_out_only_the_idle_chunk_await() {
    tokio::time::pause();
    let dropped = Arc::new(AtomicBool::new(false));
    let (sender, mut receiver) = mpsc::channel(4);
    let stream_task = tokio::spawn(stream_sse_source(
        HangingFixture::new(Arc::clone(&dropped)),
        None,
        sender,
        Duration::from_secs(90),
        DecoderKind::Responses(OpenAiComputerToolKind::Generic),
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
        .expect_err("idle deadline is typed");
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert!(
        error.retryable,
        "an idle timeout is transient; core still refuses replay after content"
    );
    assert_eq!(error.opened_within_ms, Some(90_000));
    assert_eq!(error.budget_ms, Some(90_000));
    assert!(receiver.recv().await.is_none());
    stream_task.await.expect("stream task exits");
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn openai_transport_defaults_split_connect_open_and_idle_budgets() {
    let config = OpenAiProvider::transport_config();
    assert_eq!(config.connect_timeout, Duration::from_secs(10));
    assert_eq!(config.response_open_timeout, Duration::from_secs(60));
    assert_eq!(config.chunk_idle_timeout, Duration::from_secs(90));

    let open =
        response_open_timeout_error(config.response_open_timeout, config.response_open_timeout);
    assert_eq!(open.kind, ProviderErrorKind::Transport);
    assert_eq!(open.presentation.subcode.as_str(), "provider-timeout");
    assert!(open.retryable, "response-open timeouts are transient");
    assert_eq!(open.opened_within_ms, Some(60_000));
    assert_eq!(open.budget_ms, Some(60_000));
    assert_eq!(open.presentation.opened_within_ms, Some(60_000));
    assert_eq!(open.presentation.budget_ms, Some(60_000));
    let connect = connect_timeout_error(config.connect_timeout);
    assert_eq!(connect.kind, ProviderErrorKind::Transport);
    assert_eq!(connect.presentation.subcode.as_str(), "provider-timeout");
    assert!(connect.retryable, "connect timeouts are transient");
    assert_eq!(connect.opened_within_ms, Some(10_000));
    assert_eq!(connect.budget_ms, Some(10_000));
    assert_eq!(connect.presentation.opened_within_ms, Some(10_000));
    assert_eq!(connect.presentation.budget_ms, Some(10_000));
    let idle = stream_idle_error(config.chunk_idle_timeout);
    assert_eq!(idle.kind, ProviderErrorKind::Transport);
    assert_eq!(idle.presentation.subcode.as_str(), "provider-timeout");
    assert!(idle.retryable, "stream-idle timeouts are transient");
}

#[test]
fn effective_request_budget_obeys_provider_and_run_deadlines() {
    let margin = crate::PROVIDER_DEADLINE_SAFETY_MARGIN;
    assert_eq!(
        crate::effective_request_budget(
            Duration::from_secs(60),
            Some(Duration::from_secs(15)),
            margin,
        ),
        Ok(Duration::from_secs(14)),
        "a 15s run reserves the terminalization margin from a 60s provider open"
    );
    assert_eq!(
        crate::effective_request_budget(Duration::from_secs(60), None, margin),
        Ok(Duration::from_secs(60)),
        "interactive requests retain the provider's 60s default"
    );
    assert_eq!(
        crate::effective_request_budget(
            Duration::from_secs(5),
            Some(Duration::from_secs(15)),
            margin,
        ),
        Ok(Duration::from_secs(5)),
        "a shorter provider budget remains authoritative"
    );
    assert_eq!(
        crate::effective_request_budget(
            Duration::from_secs(60),
            Some(margin.saturating_sub(Duration::from_millis(1))),
            margin,
        ),
        Err(crate::ProviderTimeoutReason::DeadlineExhausted),
        "an exhausted terminalization margin fails immediately"
    );
}

#[test]
fn retry_requires_one_complete_provider_budget_after_the_margin() {
    let margin = crate::PROVIDER_DEADLINE_SAFETY_MARGIN;
    let provider_budget = Duration::from_secs(5);
    let full =
        crate::effective_request_budget(provider_budget, Some(provider_budget + margin), margin);
    assert_eq!(full, Ok(provider_budget));
    let partial = crate::effective_request_budget(
        provider_budget,
        Some(provider_budget + margin - Duration::from_millis(1)),
        margin,
    );
    assert!(partial.is_ok_and(|selected| selected < provider_budget));
}

#[tokio::test]
async fn exhausted_run_deadline_is_an_immediate_terminal_provider_timeout() {
    let error = crate::before_provider_request_deadline(
        Some(tokio::time::Instant::now()),
        std::future::pending::<Result<(), ProviderError>>(),
    )
    .await
    .expect_err("an exhausted deadline does not poll the provider indefinitely");
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert_eq!(
        error.timeout_reason,
        Some(crate::ProviderTimeoutReason::DeadlineExhausted)
    );
    assert_eq!(error.presentation.subcode.as_str(), "provider-timeout");
    assert!(!error.retryable);
    assert_eq!(
        error.presentation.allowed_actions,
        vec![haider_protocol::error::ErrorAction::None]
    );
}

#[test]
fn response_open_budget_is_a_typed_per_provider_override() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("transport-profile");
    vault
        .put(&alias, b"transport-profile-secret")
        .expect("store credential");
    let override_config = OpenAiTransportConfig {
        response_open_timeout: Duration::from_secs(75),
        ..OPENAI_DEFAULT_TRANSPORT_CONFIG
    };
    let provider = OpenAiProvider::new(
        vault.resolve(&alias).expect("resolve credential"),
        "gpt-5.6",
    )
    .expect("construct provider")
    .with_transport_config(override_config)
    .expect("apply profile transport override");
    assert_eq!(provider.http.transport_config, override_config);

    let compatible = OpenAiCompatibleProvider::new_deepseek_api(
        vault
            .resolve(&alias)
            .expect("resolve compatible credential"),
        "deepseek-reasoner",
        DEEPSEEK_BASE_URL,
    )
    .expect("construct compatible provider")
    .with_transport_config(override_config)
    .expect("apply compatible profile transport override");
    assert_eq!(compatible.http.transport_config, override_config);
}

/// MUTATION CHECK: change the expected status or body below. Expected runtime
/// failure: the delayed response is still captured byte-exact under budget.
#[tokio::test]
async fn response_open_budget_accepts_a_mock_upstream_that_opens_at_twenty_seconds() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind slow-open fixture");
    let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (start_delay_tx, start_delay_rx) = tokio::sync::oneshot::channel();
    let (delay_armed_tx, delay_armed_rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept inference request");
        let mut request = [0_u8; 8192];
        let read = socket
            .read(&mut request)
            .await
            .expect("read inference request");
        assert!(read > 0, "fixture received an HTTP request");
        accepted_tx.send(()).expect("signal accepted request");
        start_delay_rx.await.expect("start delayed first byte");
        let delay = tokio::time::sleep(Duration::from_secs(20));
        tokio::pin!(delay);
        delay_armed_tx.send(()).expect("signal armed delay");
        delay.await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .await
            .expect("write delayed response");
    });
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("slow-open-20s");
    vault
        .put(&alias, b"slow-open-secret")
        .expect("store credential");
    let provider = OpenAiCompatibleProvider::new_custom(
        vault.resolve(&alias).expect("resolve credential"),
        "slow-open-model",
        &origin,
    )
    .expect("construct loopback provider");
    let opening = tokio::spawn(async move {
        provider
            .capture_response(&probe_request("slow-open-model"))
            .await
    });
    // Real loopback I/O must complete before virtual time is paused. Starting
    // paused lets Tokio auto-advance the 60-second transport timer while a
    // loaded macOS runner is still scheduling connect/read readiness.
    accepted_rx.await.expect("request reached slow upstream");
    tokio::time::pause();
    start_delay_tx.send(()).expect("release delayed first byte");
    delay_armed_rx.await.expect("delayed first byte is armed");
    tokio::time::advance(Duration::from_secs(19)).await;
    assert!(!opening.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.await.expect("slow-open fixture exits");
    let capture = opening
        .await
        .expect("open task exits")
        .expect("20-second response opens under the default budget");
    assert_eq!(capture.status, 200);
    assert_eq!(capture.body, b"{}");
}

/// MUTATION CHECK: change any expected `60_000` telemetry value below.
/// Expected runtime failure: the fired response-open budget remains exact in
/// both the typed error and its presentation.
#[tokio::test]
async fn response_open_budget_fails_typed_after_sixty_one_seconds() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind over-budget fixture");
    let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (start_delay_tx, start_delay_rx) = tokio::sync::oneshot::channel();
    let (delay_armed_tx, delay_armed_rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept inference request");
        let mut request = [0_u8; 8192];
        let read = socket
            .read(&mut request)
            .await
            .expect("read inference request");
        assert!(read > 0, "fixture received an HTTP request");
        accepted_tx.send(()).expect("signal accepted request");
        start_delay_rx.await.expect("start delayed first byte");
        let delay = tokio::time::sleep(Duration::from_secs(61));
        tokio::pin!(delay);
        delay_armed_tx.send(()).expect("signal armed delay");
        delay.await;
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .await;
    });
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("slow-open-61s");
    vault
        .put(&alias, b"slow-open-secret")
        .expect("store credential");
    let provider = OpenAiCompatibleProvider::new_custom(
        vault.resolve(&alias).expect("resolve credential"),
        "slow-open-model",
        &origin,
    )
    .expect("construct loopback provider");
    let opening = tokio::spawn(async move {
        provider
            .capture_response(&probe_request("slow-open-model"))
            .await
    });
    accepted_rx.await.expect("request reached slow upstream");
    tokio::time::pause();
    start_delay_tx.send(()).expect("release delayed first byte");
    delay_armed_rx.await.expect("delayed first byte is armed");
    tokio::time::advance(Duration::from_secs(60)).await;
    let error = opening
        .await
        .expect("timeout task exits")
        .expect_err("the local request deadline fires");
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert!(error.retryable);
    assert_eq!(error.opened_within_ms, Some(60_000));
    assert_eq!(error.budget_ms, Some(60_000));
    assert_eq!(error.presentation.opened_within_ms, Some(60_000));
    assert_eq!(error.presentation.budget_ms, Some(60_000));
    assert!(error.message.contains("opened_within_ms=60000"));
    assert!(error.message.contains("budget_ms=60000"));
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.await.expect("over-budget fixture exits");
}

#[tokio::test]
async fn dropping_openai_stream_aborts_its_hanging_source() {
    let dropped = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel(4);
    let producer = tokio::spawn(stream_sse_source(
        HangingFixture::new(Arc::clone(&dropped)),
        None,
        sender,
        Duration::from_secs(90),
        DecoderKind::Responses(OpenAiComputerToolKind::Generic),
    ));
    let mut stream = ProviderStream::owned(receiver, producer);
    assert!(matches!(
        stream.recv().await,
        Some(Ok(StreamEvent::TextDelta { .. }))
    ));
    drop(stream);

    tokio::time::timeout(Duration::from_secs(1), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("producer abort drops source");
}

/// MUTATION CHECK: remove the resolved-address call to
/// `validate_resolved_compatible_origin`.
///
/// Safe code rejects before `get_request` or `post_json_body_request` can
/// construct a request carrying the bearer. The mutation makes the
/// metadata-host request observable here with the exact sentinel header.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn hostname_resolution_rejects_every_forbidden_answer_before_bearer_construction() {
    let forbidden_sets = [
        vec![SocketAddr::from(([169, 254, 169, 254], 443))],
        vec![SocketAddr::from(([10, 23, 45, 67], 443))],
        vec![SocketAddr::from(([172, 16, 45, 67], 443))],
        vec![SocketAddr::from(([192, 168, 10, 20], 443))],
        vec![SocketAddr::from(([0, 0, 0, 0], 443))],
        vec![SocketAddr::new(
            "fe80::1"
                .parse::<Ipv6Addr>()
                .expect("link-local IPv6")
                .into(),
            443,
        )],
        vec![SocketAddr::new(
            "fc00::1".parse::<Ipv6Addr>().expect("ULA IPv6").into(),
            443,
        )],
        vec![SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 443)],
        vec![
            SocketAddr::from(([93, 184, 216, 34], 443)),
            SocketAddr::from(([169, 254, 169, 254], 443)),
        ],
    ];

    for addresses in forbidden_sets {
        let resolver = Arc::new(StubDnsResolver::new([addresses]));
        let provider = compatible_provider_with_resolver(
            b"resolved-origin-sentinel",
            "https://model-gateway.test",
            resolver.clone(),
        );

        assert_forbidden_origin_request(
            provider.http.get_request(&provider.models_url).await,
            "GET",
        );
        assert_forbidden_origin_request(
            provider
                .http
                .post_json_body_request(
                    &provider.chat_url,
                    serialized_json_body(serde_json::json!({"model":"audit-model"})),
                )
                .await,
            "POST",
        );
        assert_eq!(resolver.calls(), 1, "origin must resolve exactly once");
    }
}

#[tokio::test]
async fn plain_http_hostname_requires_every_resolved_address_to_be_loopback() {
    let loopback = Arc::new(StubDnsResolver::new([vec![
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 11434),
        SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 11434),
    ]]));
    let allowed = compatible_provider_with_resolver(
        b"loopback-secret",
        "http://ollama.test:11434",
        loopback.clone(),
    );
    allowed
        .http
        .validate_compatible_origin()
        .await
        .expect("loopback hostname is a valid plain-HTTP Ollama origin");
    assert_eq!(loopback.calls(), 1);

    let remote = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [93, 184, 216, 34],
        80,
    ))]]));
    let rejected = compatible_provider_with_resolver(
        b"remote-secret",
        "http://remote-gateway.test",
        remote.clone(),
    );
    let error = rejected
        .http
        .validate_compatible_origin()
        .await
        .expect_err("plain HTTP must not resolve to a non-loopback address");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("non-loopback"));
    assert_eq!(remote.calls(), 1);
}

/// MUTATION CHECK: remove the shared `validate_compatible_origin` call from
/// `compatible_endpoints`. Expected runtime failure: this management probe
/// advances to transport I/O instead of returning the actionable
/// non-loopback plain-HTTP rejection.
#[tokio::test]
async fn configured_endpoint_probe_rejects_remote_plain_http_with_guard_vocabulary() {
    let error = validate_openai_compatible_endpoint("http://203.0.113.7", Default::default())
        .await
        .expect_err("remote plain HTTP must fail before probing");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("must use HTTPS"));
    assert!(error.message.contains("HTTP"));
    assert!(error.message.contains("loopback"));
}

/// MUTATION CHECK: replace `Policy::none()` in
/// `validate_openai_compatible_endpoint` with reqwest's redirect default.
/// Expected runtime failure: this test returns the redirected final origin as
/// valid instead of reporting the actionable redirect rejection.
#[tokio::test]
async fn configured_endpoint_probe_rejects_redirects() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect fixture");
    let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
    let fixture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept probe");
        let mut request = [0_u8; 2048];
        let _ = socket.read(&mut request).await.expect("read probe");
        socket
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/v1/models\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write redirect");
    });
    let error = validate_openai_compatible_endpoint(&origin, Default::default())
        .await
        .expect_err("redirects must not be followed");
    fixture.await.expect("redirect fixture");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("redirects are not allowed"));
    assert!(error.message.contains("final origin"));
}

#[tokio::test]
async fn validated_hostname_addresses_are_pinned_through_connection_establishment() {
    let target = SocketAddr::from(([127, 0, 0, 1], 0));
    let resolver = Arc::new(StubDnsResolver::new([
        vec![target],
        vec![SocketAddr::from(([169, 254, 169, 254], 0))],
    ]));
    let provider = compatible_provider_with_resolver(
        b"rebind-pin-sentinel",
        "http://ollama-rebind.test:0",
        resolver.clone(),
    );

    provider
        .http
        .validate_compatible_origin()
        .await
        .expect("first resolver answer is valid loopback");
    let guard = provider
        .http
        .origin_guard
        .as_ref()
        .expect("hostname origin has a pinning guard");
    let pinned = guard
        .validated
        .get()
        .expect("origin validation populated the pin cache")
        .as_ref()
        .expect("pin cache contains accepted addresses");
    let request = provider
        .http
        .get_request(&provider.models_url)
        .await
        .expect("validated origin builds capability request");

    assert_eq!(pinned.as_ref(), &[target]);
    assert_eq!(request.url().host_str(), Some("ollama-rebind.test"));
    assert_eq!(request.url().port(), Some(0));
    assert_eq!(
        request
            .headers()
            .get(AUTHORIZATION)
            .expect("allowed pinned request carries bearer"),
        "Bearer rebind-pin-sentinel"
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        provider.http.client.execute(request),
    )
    .await
    .expect("impossible loopback connection fails promptly")
    .expect_err("port zero cannot accept the pinned connection");
    assert_eq!(
        resolver.calls(),
        1,
        "reqwest connection must use the validated cache, not resolve again"
    );
    assert_eq!(
        guard.connection_lookups.load(Ordering::SeqCst),
        1,
        "reqwest itself must consume the installed pinned resolver"
    );
}

/// MUTATION CHECK: remove `.no_proxy()` from `OpenAiHttp` construction.
///
/// The child receives explicit proxy environment variables without mutating
/// this test process. Safe code's built client has no proxy matcher while its
/// request still targets the pinned hostname. The mutation exposes the
/// inherited proxy matcher in reqwest's inspectable client configuration.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn compatible_credential_client_ignores_inherited_proxy_environment() {
    const CHILD_MARKER: &str = "HAIDER_PROXY_PIN_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let target = SocketAddr::from(([127, 0, 0, 1], 11434));
        let resolver = Arc::new(StubDnsResolver::new([vec![target]]));
        let provider = compatible_provider_with_resolver(
            b"proxy-off-sentinel",
            "http://proxy-pin.test:11434",
            resolver,
        );
        let request = provider
            .http
            .get_request(&provider.models_url)
            .await
            .expect("proxy-off client builds pinned request");
        assert_eq!(request.url().host_str(), Some("proxy-pin.test"));
        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .expect("allowed loopback request carries bearer"),
            "Bearer proxy-off-sentinel"
        );
        assert!(
            !format!("{:?}", provider.http.client).contains("proxies"),
            "credential-bearing client retained inherited proxy configuration"
        );
        let vault = MemoryVault::new();
        let alias = CredentialAlias::new("native-proxy-audit");
        vault
            .put(&alias, b"native-proxy-sentinel")
            .expect("store native proxy audit secret");
        let credential = vault
            .resolve(&alias)
            .expect("resolve native proxy audit secret");
        let native = OpenAiProvider::new(credential, "gpt-audit").expect("native OpenAI client");
        assert!(
            !format!("{:?}", native.http.client).contains("proxies"),
            "native OpenAI credential-bearing client retained inherited proxy configuration"
        );
        let subscription_vault = MemoryVault::new();
        let subscription_alias = CredentialAlias::new("subscription-proxy-audit");
        subscription_vault
            .put(&subscription_alias, b"subscription-proxy-sentinel")
            .expect("store subscription proxy audit secret");
        let subscription = OpenAiProvider::new_subscription(
            subscription_vault
                .resolve(&subscription_alias)
                .expect("resolve subscription proxy secret"),
            "gpt-audit",
            OPENAI_SUBSCRIPTION_BASE_URL,
        )
        .expect("subscription OpenAI client");
        assert!(
            !format!("{:?}", subscription.http.client).contains("proxies"),
            "subscription OpenAI client retained inherited proxy configuration"
        );
        return;
    }

    let output =
        tokio::process::Command::new(std::env::current_exe().expect("current test binary"))
            .arg("compatible_credential_client_ignores_inherited_proxy_environment")
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
            .await
            .expect("run isolated proxy child");

    assert!(
        output.status.success(),
        "proxy child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// MUTATION CHECK: restore api.openai.com, remove the Codex-lite header, use
/// x-api-key, point the fixed endpoint at a private host, or remove the pinned
/// fixed resolver. Each mutation fails a named assertion before any
/// credential-bearing network request can be sent.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn openai_oauth_subscription_is_codex_bearer_lite_and_fixed_origin() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("openai-oauth-request-audit");
    vault
        .put(&alias, b"OPENAI_OAUTH_ACCESS_SENTINEL_4fd8")
        .expect("store OAuth access");
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [93, 184, 216, 34],
        443,
    ))]]));
    let provider = OpenAiProvider::new_subscription_with_dns_resolver(
        vault.resolve(&alias).expect("resolve OAuth access"),
        "gpt-audit",
        OPENAI_SUBSCRIPTION_BASE_URL,
        resolver.clone(),
    )
    .expect("OpenAI subscription provider");
    let request = provider
        .http
        .post_json_body_request(
            &provider.api_url,
            serialized_json_body(serde_json::json!({"model":"gpt-audit"})),
        )
        .await
        .expect("fixed request");
    assert_eq!(
        request.url().as_str(),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        request.headers().get(AUTHORIZATION).expect("Bearer header"),
        "Bearer OPENAI_OAUTH_ACCESS_SENTINEL_4fd8"
    );
    assert_eq!(
        request
            .headers()
            .get(OPENAI_CODEX_RESPONSES_LITE_HEADER)
            .expect("Codex-lite header"),
        "true"
    );
    assert!(!request.headers().contains_key("x-api-key"));
    assert_eq!(resolver.calls(), 1, "fixed host resolves once before build");
    let fixed_guard = provider
        .http
        .fixed_origin_guard
        .as_ref()
        .expect("subscription provider has a fixed-origin guard");
    fixed_guard.stall_connection_resolution();
    let execution = provider.http.client.execute(request);
    tokio::pin!(execution);
    let resolution_observed = async {
        while fixed_guard.connection_resolution_count() == 0 {
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
        fixed_guard.connection_resolution_count(),
        1,
        "one connection lookup must use the pinned fixed resolver"
    );

    let private_vault = MemoryVault::new();
    let private_alias = CredentialAlias::new("openai-private-origin-audit");
    private_vault
        .put(&private_alias, b"NEVER_SEND_OPENAI_PRIVATE_9c12")
        .expect("store private-origin sentinel");
    let rejected = OpenAiHttp::new_subscription(
        private_vault
            .resolve(&private_alias)
            .expect("resolve private-origin sentinel"),
        "gpt-audit",
        "http://169.254.169.254/backend-api/codex/responses",
        Arc::new(StubDnsResolver::new([])),
    )
    .expect_err("private fixed endpoint must be rejected");
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
        let rebound_alias = CredentialAlias::new("openai-rebound-origin-audit");
        rebound_vault
            .put(&rebound_alias, b"NEVER_SEND_OPENAI_REBOUND_18de")
            .expect("store rebound sentinel");
        let rebound = OpenAiProvider::new_subscription_with_dns_resolver(
            rebound_vault
                .resolve(&rebound_alias)
                .expect("resolve rebound sentinel"),
            "gpt-audit",
            OPENAI_SUBSCRIPTION_BASE_URL,
            Arc::new(StubDnsResolver::new([vec![rebound_address]])),
        )
        .expect("construct fixed-host rebound audit");
        let rebound_error = rebound
            .http
            .post_json_body_request(
                &rebound.api_url,
                serialized_json_body(serde_json::json!({"model":"gpt-audit"})),
            )
            .await
            .expect_err("loopback/private DNS answer must fail before bearer construction");
        assert_eq!(rebound_error.kind, ProviderErrorKind::InvalidRequest);
    }
}

/// MUTATION CHECK: remove `validate_compatible_origin(&parsed)?`.
///
/// The fallback branch deliberately routes the exact metadata URL through
/// a local proxy and proves the bearer crosses the wire. Safe code returns
/// before any client, socket, or authorization header is constructed.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn metadata_origin_guard_prevents_credential_bearing_request() {
    let endpoint_result = compatible_endpoints("http://169.254.169.254", Default::default());
    if let Err(error) = endpoint_result {
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        return;
    }

    let endpoints = endpoint_result.expect("mutation removed metadata origin guard");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mutation-only audit proxy");
    let proxy_url = format!("http://{}", listener.local_addr().expect("proxy address"));
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("metadata-audit");
    vault
        .put(&alias, b"metadata-sentinel-secret")
        .expect("store audit secret");
    let credential = vault.resolve(&alias).expect("resolve audit secret");
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&proxy_url).expect("audit proxy"))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .expect("audit client");
    let provider = OpenAiCompatibleProvider {
        http: OpenAiHttp {
            client,
            credential,
            account: Some(alias),
            model: "audit-model".into(),
            origin_guard: None,
            fixed_origin_guard: None,
            codex_responses_lite: false,
            auth_header_mode: OpenAiAuthHeaderMode::Bearer,
            grok_subscription_headers: false,
            transport_config: OPENAI_DEFAULT_TRANSPORT_CONFIG,
        },
        base_url: endpoints.base_url,
        chat_url: endpoints.chat_url,
        models_url: endpoints.models_url,
        dialect: CompatibleDialect::Generic,
        catalog_model: None,
        kimi_thinking: None,
        kimi_reasoning_effort: None,
    };
    let capture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("proxy request");
        let mut request = vec![0_u8; 4096];
        let read = socket.read(&mut request).await.expect("read proxy request");
        request.truncate(read);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 47\r\nConnection: close\r\n\r\n{\"data\":[{\"id\":\"audit-model\"}],\"object\":\"list\"}",
            )
            .await
            .expect("write proxy response");
        request
    });
    provider
        .probe_capabilities()
        .await
        .expect("mutated request reaches proxy");
    let request = String::from_utf8(capture.await.expect("proxy task")).expect("HTTP text");
    assert!(request.contains("GET http://169.254.169.254/v1/models"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer metadata-sentinel-secret")
    );
    panic!("credential-bearing metadata request observed after origin-check mutation");
}

fn compatible_provider_with_resolver(
    secret: &[u8],
    base_url: impl AsRef<str>,
    resolver: Arc<StubDnsResolver>,
) -> OpenAiCompatibleProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("resolved-origin-audit");
    vault.put(&alias, secret).expect("store audit secret");
    let credential = vault.resolve(&alias).expect("resolve audit secret");
    OpenAiCompatibleProvider::new_with_dns_resolver(credential, "audit-model", base_url, resolver)
        .expect("construct compatible provider")
        .with_account(alias)
}

/// A session turn already carries its pinned wire model. Generic compatible
/// capability resolution must therefore remain local: its `/models` shape
/// contributes only membership, and an invalid pinned model belongs to the
/// subsequent chat response.
///
/// MUTATION CHECK: restore `probe_capabilities()` for the generic dialect.
/// Expected runtime failure: the resolver call count becomes one before the
/// guarded models request is refused.
#[tokio::test]
async fn pinned_compatible_capabilities_do_not_discover_models() {
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [169, 254, 169, 254],
        443,
    ))]]));
    let provider = compatible_provider_with_resolver(
        b"pinned-model-sentinel",
        "https://models-pinned.test",
        Arc::clone(&resolver),
    );

    let capabilities = Provider::capabilities(&provider).await;
    assert_eq!(
        capabilities,
        compatible_capabilities(OPENAI_COMPATIBLE_PROVIDER_NAME)
    );
    assert_eq!(resolver.calls(), 0, "pinned turns must not discover");
}

/// Kimi and Grok publish richer per-model facts than a generic compatible
/// list. Pinned turns consume those facts from the daemon's typed catalog
/// cache and still perform no request-time discovery.
#[tokio::test]
async fn pinned_subscription_capabilities_use_cached_catalog_without_discovery() {
    let kimi_model = crate::parse_catalog(
        CatalogSource::KimiOAuth,
        &serde_json::from_slice(include_bytes!("../tests/fixtures/catalog/kimi_models.json"))
            .expect("decode Kimi catalog fixture"),
    )
    .expect("parse Kimi catalog fixture")
    .into_iter()
    .find(|model| model.slug == "kimi-coding-a")
    .expect("Kimi fixture model");
    let kimi_resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [169, 254, 169, 254],
        443,
    ))]]));
    let kimi_vault = MemoryVault::new();
    let kimi_alias = CredentialAlias::new("pinned-kimi-capabilities");
    kimi_vault
        .put(&kimi_alias, b"KIMI_PINNED_SENTINEL_2c91")
        .expect("store Kimi fixture credential");
    let kimi = OpenAiCompatibleProvider::new_kimi_subscription_with_dns_resolver(
        kimi_vault
            .resolve(&kimi_alias)
            .expect("resolve Kimi fixture credential"),
        "kimi-coding-a",
        KIMI_OAUTH_BASE_URL,
        Arc::clone(&kimi_resolver) as Arc<dyn FixedDnsResolver>,
    )
    .expect("construct Kimi adapter")
    .with_cached_catalog_model(Some(&kimi_model));
    assert_eq!(
        Provider::capabilities(&kimi).await,
        kimi_capabilities_from_model(&kimi_model).expect("Kimi cached capabilities")
    );
    assert_eq!(kimi_resolver.calls(), 0, "pinned Kimi must not discover");

    let grok_model = crate::parse_catalog(
        CatalogSource::GrokOAuth,
        &serde_json::json!({"data": [{
            "id": "grok-pinned",
            "context_window": 500_000,
            "supports_reasoning_effort": true
        }]}),
    )
    .expect("parse Grok catalog fixture")
    .into_iter()
    .next()
    .expect("Grok fixture model");
    let grok_resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [169, 254, 169, 254],
        443,
    ))]]));
    let grok_vault = MemoryVault::new();
    let grok_alias = CredentialAlias::new("pinned-grok-capabilities");
    grok_vault
        .put(&grok_alias, b"GROK_PINNED_SENTINEL_a032")
        .expect("store Grok fixture credential");
    let grok = OpenAiCompatibleProvider::new_grok_subscription_with_dns_resolver(
        grok_vault
            .resolve(&grok_alias)
            .expect("resolve Grok fixture credential"),
        "grok-pinned",
        GROK_OAUTH_BASE_URL,
        Arc::clone(&grok_resolver) as Arc<dyn FixedDnsResolver>,
    )
    .expect("construct Grok adapter")
    .with_cached_catalog_model(Some(&grok_model));
    assert_eq!(
        Provider::capabilities(&grok).await,
        grok_capabilities_from_model(&grok_model)
    );
    assert_eq!(grok_resolver.calls(), 0, "pinned Grok must not discover");
}

/// LAW (Q no-auth): a custom adapter configured without authentication emits
/// no credential header on either model discovery or inference. The internal
/// construction handle must never become an authentication header on wire.
///
/// MUTATION CHECK: route `new_custom_no_auth` through the bearer mode or add
/// either credential header. Expected RUNTIME failure: the absence assertions.
#[tokio::test]
async fn custom_no_auth_get_and_post_have_no_credential_headers() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("ollama-keyless");
    vault.put(&alias, b"ollama").expect("store placeholder");
    let credential = vault.resolve(&alias).expect("resolve placeholder");
    let provider = OpenAiCompatibleProvider::new_custom_no_auth(
        credential,
        "llama3.1:8b",
        "http://127.0.0.1:11434",
    )
    .expect("custom loopback adapter")
    .with_account(alias);

    let get = provider
        .http
        .get_request(&provider.models_url)
        .await
        .expect("models probe request");
    assert!(!get.headers().contains_key(AUTHORIZATION));
    assert!(!get.headers().contains_key("api-key"));

    let post = provider
        .http
        .post_json_body_request(
            &provider.chat_url,
            serialized_json_body(serde_json::json!({"model":"llama3.1:8b"})),
        )
        .await
        .expect("chat POST request");
    assert!(!post.headers().contains_key(AUTHORIZATION));
    assert!(!post.headers().contains_key("api-key"));
}

fn custom_provider_with_resolver(
    secret: &[u8],
    base_url: impl AsRef<str>,
    resolver: Arc<StubDnsResolver>,
) -> OpenAiCompatibleProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("custom-lan-audit");
    vault.put(&alias, secret).expect("store audit secret");
    let credential = vault.resolve(&alias).expect("resolve audit secret");
    OpenAiCompatibleProvider::new_with_policy_and_dns_resolver(
        credential,
        "audit-model",
        base_url,
        CompatibleOriginPolicy::TrustedLan,
        resolver,
    )
    .expect("construct custom compatible provider")
    .with_account(alias)
}

/// LAW (LK3 — resolved-address half of the origin matrix): under the
/// CUSTOM-provenance `TrustedLan` policy a hostname that resolves to an
/// RFC1918 LAN address is a VALID plain-HTTP origin (a LAN Ollama box),
/// while resolutions to link-local metadata, IPv6 ULA, or a public address
/// over plain HTTP stay refused. The builtin strict policy is pinned
/// unchanged by `hostname_resolution_rejects_every_forbidden_answer_*` and
/// `plain_http_hostname_requires_every_resolved_address_to_be_loopback`
/// above (same resolver harness, `new` constructor).
///
/// MUTATION CHECK (wrongly-blocked direction): make `TrustedLan` behave like
/// `Strict` in `blocked_credential_target_with_policy`. Expected RUNTIME
/// failure: the allowed RFC1918 case below errors.
/// MUTATION CHECK (wrongly-allowed direction): exempt link-local from the
/// TrustedLan block (extend `rfc1918_private` with `is_link_local`).
/// Expected RUNTIME failure: the 169.254.169.254 refusal below passes
/// validation.
#[tokio::test]
async fn lk3_custom_lan_hostname_resolution_matrix_pins_both_directions() {
    // ALLOWED: plain-HTTP hostname resolving to RFC1918 (all three ranges).
    for lan in [
        SocketAddr::from(([192, 168, 1, 20], 11434)),
        SocketAddr::from(([10, 23, 45, 67], 11434)),
        SocketAddr::from(([172, 16, 45, 67], 11434)),
    ] {
        let resolver = Arc::new(StubDnsResolver::new([vec![lan]]));
        let provider =
            custom_provider_with_resolver(b"lan-secret", "http://ollama.lan:11434", resolver);
        provider
            .http
            .validate_compatible_origin()
            .await
            .unwrap_or_else(|error| panic!("RFC1918 {lan} must be a valid custom origin: {error}"));
    }

    // `.local` is an explicitly supported trusted-LAN spelling. It is not a
    // suffix exemption: acceptance still depends on every pinned answer being
    // loopback or RFC1918.
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [192, 168, 50, 9],
        8080,
    ))]]));
    let provider =
        custom_provider_with_resolver(b"local-secret", "http://router.local:8080", resolver);
    provider
        .http
        .validate_compatible_origin()
        .await
        .expect("private-resolution router.local must be a valid custom origin");

    // REFUSED: link-local metadata, IPv6 ULA/link-local, and public plain
    // HTTP — the scoped loosening must not widen past RFC1918.
    let refused: [(SocketAddr, &str); 4] = [
        (
            SocketAddr::from(([169, 254, 169, 254], 11434)),
            "link-local metadata",
        ),
        (
            SocketAddr::new(
                "fe80::1".parse::<Ipv6Addr>().expect("link-local").into(),
                11434,
            ),
            "IPv6 link-local",
        ),
        (
            SocketAddr::new("fc00::1".parse::<Ipv6Addr>().expect("ULA").into(), 11434),
            "IPv6 ULA",
        ),
        (
            SocketAddr::from(([93, 184, 216, 34], 80)),
            "public plain HTTP",
        ),
    ];
    for (address, case) in refused {
        let resolver = Arc::new(StubDnsResolver::new([vec![address]]));
        let provider =
            custom_provider_with_resolver(b"lan-secret", "http://ollama.lan:11434", resolver);
        let Err(error) = provider.http.validate_compatible_origin().await else {
            panic!("{case} resolution must stay refused under TrustedLan");
        };
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{case}");
    }

    // A public HTTPS hostname stays valid under TrustedLan (unchanged).
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [93, 184, 216, 34],
        443,
    ))]]));
    let provider =
        custom_provider_with_resolver(b"lan-secret", "https://gateway.example.com", resolver);
    provider
        .http
        .validate_compatible_origin()
        .await
        .expect("public HTTPS hostname stays valid under TrustedLan");
}

fn assert_forbidden_origin_request(result: Result<reqwest::Request, ProviderError>, method: &str) {
    match result {
        Err(error) => {
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
            assert!(!error.retryable);
        }
        Ok(request) => {
            assert_eq!(request.method().as_str(), method);
            assert_eq!(request.url().host_str(), Some("model-gateway.test"));
            assert_eq!(
                request
                    .headers()
                    .get(AUTHORIZATION)
                    .expect("mutated request carries bearer"),
                "Bearer resolved-origin-sentinel"
            );
            panic!("credential-bearing request was built for a forbidden resolved origin");
        }
    }
}

/// W5a.2 confirm P2: `host_str()` returns bracketed IPv6 (`[::1]`), so the
/// literal-vs-hostname classification must strip the brackets — otherwise a
/// safe IPv6-literal endpoint is misread as a hostname and fails a request-time
/// DNS lookup. Fail-closed (usability, not SSRF), but IPv6-literal compatible
/// endpoints must work.
///
/// MUTATION CHECK: revert the bracket-strip (classify on the raw `host_str()`).
/// Expected failure: the loopback and public IPv6-literal cases below become
/// `Some` (hostname) instead of `None` (pre-validated literal).
#[test]
fn ipv6_literal_compatible_base_url_is_classified_as_a_literal_not_a_hostname() {
    // Loopback IPv6 over plain HTTP (the local-runtime case) is a literal:
    // origin is None (validated inline, no request-time DNS).
    let loopback = compatible_endpoints("http://[::1]:11434", Default::default())
        .expect("bracketed IPv6 loopback is a valid literal origin");
    assert!(
        loopback.origin.is_none(),
        "[::1] must classify as an IP literal, not a DNS-resolved hostname"
    );

    // A public IPv6 literal over HTTPS is likewise a literal.
    let public = compatible_endpoints("https://[2606:4700:4700::1111]:443", Default::default())
        .expect("bracketed public IPv6 literal is valid");
    assert!(
        public.origin.is_none(),
        "a public IPv6 literal must classify as a literal"
    );

    // A real hostname (no brackets) still takes the resolve-validate-pin path.
    let domain = compatible_endpoints("https://gateway.example.com:8443", Default::default())
        .expect("hostname base_url is valid");
    assert!(
        domain.origin.is_some(),
        "a domain must classify as a hostname for resolve-validate-pin"
    );
}

fn probe_request(model: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![crate::Message::user_text("say PINGACK")],
        model: model.to_owned(),
        max_tokens: 200_000,
        system_prompt: Some("Be terse.".to_owned()),
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    }
}

fn computer_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "computer".into(),
        description: "neutral computer tool".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"action": {"type": "string"}},
            "required": ["action"],
        }),
    }
}

#[test]
fn native_computer_negotiation_pins_preview_ga_generic_and_lite_shapes() {
    for model in ["computer-use-preview", "computer-use-preview-2025-03-11"] {
        let mut preview = probe_request(model);
        preview.tools = vec![computer_tool_definition()];
        let preview = responses_request_json(&preview, false, None, false).expect("preview wire");
        assert_eq!(
            preview["tools"],
            serde_json::json!([{
                "type": "computer_use_preview",
                "display_width": 2048,
                "display_height": 1152,
                "environment": openai_computer_environment(),
            }])
        );
        assert_eq!(preview["truncation"], "auto");
    }

    for model in ["gpt-5.4", "gpt-5.5-2026-06-01", "gpt-5.6-sol"] {
        let mut request = probe_request(model);
        request.tools = vec![computer_tool_definition()];
        let payload =
            responses_request_json(&request, false, None, false).expect("GA computer wire");
        assert_eq!(payload["tools"], serde_json::json!([{"type": "computer"}]));
    }

    for model in [
        "gpt-5.3",
        "gpt-5.7",
        "gpt-6.0",
        "future-computer-model",
        "computer-use-preview-future",
    ] {
        let mut generic_request = probe_request(model);
        generic_request.tools = vec![computer_tool_definition()];
        let generic = responses_request_json(&generic_request, false, None, false)
            .expect("generic fallback wire");
        assert_eq!(generic["tools"][0]["type"], "function");
        assert_eq!(generic["tools"][0]["name"], "computer");
        assert_eq!(generic["tools"][0]["strict"], false);
        assert_eq!(generic["tools"][0]["description"], "neutral computer tool");
        assert_eq!(
            generic["tools"][0]["parameters"],
            computer_tool_definition().input_schema
        );
    }

    let mut lite_request = probe_request("gpt-5.6");
    lite_request.tools = vec![computer_tool_definition()];
    let lite = responses_request_json(&lite_request, true, None, false).expect("lite generic wire");
    assert_eq!(lite["tools"][0]["type"], "function");
    assert_eq!(lite["tools"][0]["name"], "computer");
}

#[test]
fn native_computer_call_decodes_preview_and_ga_batches_to_singular_actions() {
    let preview_item = serde_json::json!({
        "type": "computer_call",
        "id": "cu_preview_item",
        "call_id": "cu_preview",
        "action": {"type": "click", "button": "left", "x": 420, "y": 240},
        "pending_safety_checks": [{"id": "safe_1", "code": "policy"}],
        "status": "completed",
    });
    let preview_wire = format!(
        "event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": preview_item}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_preview"}}),
    );
    let generic_events = replay_openai_responses_sse(preview_wire.as_bytes());
    assert!(!generic_events.iter().any(|item| matches!(
        item,
        Ok(StreamEvent::ProviderOpaque { .. } | StreamEvent::ToolCallStart { .. })
    )));
    assert_eq!(
        generic_events.last(),
        Some(&Ok(StreamEvent::Finish {
            reason: FinishReason::EndTurn,
        }))
    );

    let preview_events =
        replay_openai_native_computer_sse(preview_wire.as_bytes(), "computer-use-preview");
    let preview_args = preview_events
        .iter()
        .filter_map(|item| match item {
            Ok(StreamEvent::ToolCallArgsDelta { args_fragment, .. }) => {
                Some(serde_json::from_str::<serde_json::Value>(args_fragment).expect("args JSON"))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        preview_args,
        [
            serde_json::json!({"action": "left_click", "x": 420, "y": 240}),
            serde_json::json!({"action": "screenshot"}),
        ]
    );
    assert!(preview_events.contains(&Ok(StreamEvent::ProviderOpaque {
        provider: OPENAI_PROVIDER_NAME.into(),
        data: preview_item,
    })));
    assert_eq!(
        preview_events.last(),
        Some(&Ok(StreamEvent::Finish {
            reason: FinishReason::ToolUse,
        }))
    );

    let ga_item = serde_json::json!({
        "type": "computer_call",
        "call_id": "cu_ga",
        "actions": [
            {"type": "keypress", "keys": ["CTRL", "L"]},
            {"type": "type", "text": "https://example.com"},
        ],
        "status": "completed",
    });
    let ga_wire = format!(
        "event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": ga_item}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_ga"}}),
    );
    let ga_actions = replay_openai_native_computer_sse(ga_wire.as_bytes(), "gpt-5.4")
        .into_iter()
        .filter_map(|item| match item {
            Ok(StreamEvent::ToolCallArgsDelta { args_fragment, .. }) => {
                Some(serde_json::from_str::<serde_json::Value>(&args_fragment).expect("args JSON"))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ga_actions,
        [
            serde_json::json!({"action": "key", "keys": "CTRL+L"}),
            serde_json::json!({"action": "type", "text": "https://example.com"}),
            serde_json::json!({"action": "screenshot"}),
        ]
    );
}

#[test]
fn native_computer_action_translation_covers_the_supported_openai_vocabulary() {
    let translate = |action: serde_json::Value| {
        openai_computer_actions(&action)
            .expect("supported native action")
            .into_iter()
            .map(|action| serde_json::to_value(action).expect("normalized action JSON"))
            .collect::<Vec<_>>()
    };

    for lossy in [
        serde_json::json!({
            "type": "click", "button": "left", "x": 1, "y": 2, "keys": ["SHIFT"]
        }),
        serde_json::json!({
            "type": "scroll", "x": 1, "y": 2, "scroll_x": 0, "scroll_y": 100,
            "keys": ["CTRL"]
        }),
        serde_json::json!({
            "type": "click", "button": "left", "x": 1, "y": 2, "keys": "SHIFT"
        }),
    ] {
        assert!(
            openai_computer_actions(&lossy).is_err(),
            "native modifier input must not be silently weakened: {lossy}"
        );
    }

    assert_eq!(
        translate(serde_json::json!({
            "type": "click", "button": "right", "x": 7, "y": 8
        })),
        [
            serde_json::json!({"action": "mouse_move", "x": 7, "y": 8}),
            serde_json::json!({"action": "right_click"}),
        ]
    );
    assert_eq!(
        translate(serde_json::json!({
            "type": "double_click", "x": 9, "y": 10
        })),
        [
            serde_json::json!({"action": "mouse_move", "x": 9, "y": 10}),
            serde_json::json!({"action": "double_click"}),
        ]
    );
    assert_eq!(
        translate(serde_json::json!({
            "type": "drag",
            "path": [{"x": 1, "y": 2}, {"x": 4, "y": 5}, {"x": 7, "y": 8}]
        })),
        [
            serde_json::json!({"action": "mouse_move", "x": 1, "y": 2}),
            serde_json::json!({"action": "left_mouse_down"}),
            serde_json::json!({"action": "mouse_move", "x": 4, "y": 5}),
            serde_json::json!({"action": "mouse_move", "x": 7, "y": 8}),
            serde_json::json!({"action": "left_mouse_up"}),
        ]
    );
    assert_eq!(
        translate(serde_json::json!({
            "type": "drag",
            "path": [[1, 2], [7, 8]]
        })),
        [serde_json::json!({
            "action": "left_click_drag",
            "from": {"x": 1, "y": 2},
            "to": {"x": 7, "y": 8}
        })]
    );
    assert_eq!(
        translate(serde_json::json!({
            "type": "scroll", "x": 20, "y": 30, "scroll_x": -250, "scroll_y": 151
        })),
        [
            serde_json::json!({
                "action": "scroll", "x": 20, "y": 30, "direction": "down", "amount": 2
            }),
            serde_json::json!({
                "action": "scroll", "x": 20, "y": 30, "direction": "left", "amount": 3
            }),
        ]
    );
    assert_eq!(
        translate(serde_json::json!({"type": "keypress", "keys": ["CTRL", "L"]})),
        [serde_json::json!({"action": "key", "keys": "CTRL+L"})]
    );
    assert_eq!(
        translate(serde_json::json!({"type": "type", "text": "hello"})),
        [serde_json::json!({"action": "type", "text": "hello"})]
    );
    assert_eq!(
        translate(serde_json::json!({"type": "move", "x": 11, "y": 12})),
        [serde_json::json!({"action": "mouse_move", "x": 11, "y": 12})]
    );
    assert_eq!(
        translate(serde_json::json!({"type": "wait"})),
        [serde_json::json!({"action": "wait", "ms": 2000})]
    );
    assert_eq!(
        translate(serde_json::json!({"type": "screenshot"})),
        [serde_json::json!({"action": "screenshot"})]
    );

    for rejected in [
        serde_json::json!({
            "type": "click", "button": "left", "x": 1, "y": 2, "keys": ["SHIFT"]
        }),
        serde_json::json!({"type": "drag", "path": [{"x": 1, "y": 2}]}),
        serde_json::json!({
            "type": "scroll", "x": 1, "y": 2, "scroll_x": 0, "scroll_y": 0
        }),
        serde_json::json!({"type": "zoom", "x": 1, "y": 2}),
    ] {
        assert!(openai_computer_actions(&rejected).is_err());
    }
}

fn native_computer_followup(model: &str, call: serde_json::Value) -> TurnRequest {
    let provider_call_id = call["call_id"].as_str().expect("native call id").to_owned();
    let native_actions = if let Some(actions) = call["actions"].as_array() {
        actions.iter().collect::<Vec<_>>()
    } else {
        vec![&call["action"]]
    };
    let mut normalized_actions = native_actions
        .into_iter()
        .flat_map(|action| openai_computer_actions(action).expect("native action translates"))
        .collect::<Vec<_>>();
    if !matches!(normalized_actions.last(), Some(ComputerAction::Screenshot)) {
        normalized_actions.push(ComputerAction::Screenshot);
    }
    let action_count = normalized_actions.len();
    let artifact = ArtifactRef::new(format!("blake3:{provider_call_id}"));
    let mut assistant_blocks = vec![Block::ProviderOpaque {
        provider: OPENAI_PROVIDER_NAME.into(),
        data: call,
    }];
    for index in 0..action_count {
        assistant_blocks.push(Block::ToolCall {
            call_id: native_computer_action_call_id(&provider_call_id, index),
            name: "computer".into(),
            args: if index + 1 == action_count {
                serde_json::json!({"action": "screenshot"})
            } else {
                serde_json::json!({"action": "wait", "ms": 2000})
            },
        });
    }
    let mut messages = vec![Message::assistant(assistant_blocks)];
    for index in 0..action_count {
        let call_id = native_computer_action_call_id(&provider_call_id, index);
        if index + 1 == action_count {
            messages.push(Message::tool_result_with_images(
                call_id,
                "captured",
                false,
                vec![ImageBlockRef {
                    artifact: artifact.clone(),
                    media_type: "image/png".into(),
                    width: 1_600,
                    height: 900,
                    byte_len: 8,
                }],
            ));
        } else {
            messages.push(Message::tool_result(call_id, "ok", false));
        }
    }
    TurnRequest {
        messages,
        model: model.into(),
        max_tokens: 256,
        system_prompt: None,
        tools: vec![computer_tool_definition()],
        attachments: vec![crate::ResolvedAttachment {
            artifact,
            data_base64: "iVBORw0=".into(),
        }],
        cache_metadata: None,
    }
}

#[test]
fn native_computer_results_group_under_provider_call_and_preserve_contract() {
    let preview_call = serde_json::json!({
        "type": "computer_call",
        "call_id": "preview_group",
        "action": {"type": "click", "button": "left", "x": 10, "y": 20},
        "pending_safety_checks": [{"id": "safe_1", "code": "policy"}],
        "status": "completed",
    });
    let preview = responses_request_json(
        &native_computer_followup("computer-use-preview", preview_call.clone()),
        false,
        None,
        false,
    )
    .expect("preview follow-up");
    assert_eq!(preview["tools"][0]["display_width"], 1_600);
    assert_eq!(preview["tools"][0]["display_height"], 900);
    assert_eq!(preview["input"][0], preview_call);
    assert_eq!(preview["input"].as_array().expect("input").len(), 2);
    assert_eq!(preview["input"][1]["type"], "computer_call_output");
    assert_eq!(preview["input"][1]["call_id"], "preview_group");
    assert_eq!(
        preview["input"][1]["acknowledged_safety_checks"],
        serde_json::json!([{"id": "safe_1", "code": "policy"}])
    );
    assert_eq!(
        preview["input"][1]["output"],
        serde_json::json!({
            "type": "computer_screenshot",
            "image_url": "data:image/png;base64,iVBORw0=",
        })
    );

    let ga_call = serde_json::json!({
        "type": "computer_call",
        "call_id": "ga_group",
        "actions": [{"type": "wait"}],
        "status": "completed",
    });
    let ga = responses_request_json(
        &native_computer_followup("gpt-5.4", ga_call.clone()),
        false,
        None,
        false,
    )
    .expect("GA follow-up");
    assert_eq!(ga["tools"], serde_json::json!([{"type": "computer"}]));
    assert_eq!(ga["input"][0], ga_call);
    assert_eq!(ga["input"].as_array().expect("input").len(), 2);
    assert_eq!(
        ga["input"][1],
        serde_json::json!({
            "type": "computer_call_output",
            "call_id": "ga_group",
            "output": {
                "type": "computer_screenshot",
                "image_url": "data:image/png;base64,iVBORw0=",
                "detail": "original",
            }
        })
    );

    let expanded_call = serde_json::json!({
        "type": "computer_call",
        "call_id": "expanded_group",
        "actions": [
            {"type": "click", "button": "right", "x": 10, "y": 20},
            {"type": "scroll", "x": 10, "y": 20, "scroll_x": -200, "scroll_y": 200}
        ],
        "status": "completed",
    });
    let expanded = responses_request_json(
        &native_computer_followup("gpt-5.6", expanded_call.clone()),
        false,
        None,
        false,
    )
    .expect("expanded GA follow-up");
    assert_eq!(expanded["input"][0], expanded_call);
    assert_eq!(expanded["input"].as_array().expect("input").len(), 2);
    assert_eq!(expanded["input"][1]["call_id"], "expanded_group");
    assert_eq!(
        expanded["input"][1]["output"]["type"],
        "computer_screenshot"
    );
}

#[test]
fn native_computer_result_shaping_does_not_hide_denied_or_failed_actions() {
    for status in ["denied", "rejected", "cancelled", "failed"] {
        let call = serde_json::json!({
            "type": "computer_call",
            "call_id": format!("blocked_{status}"),
            "actions": [{"type": "click", "button": "left", "x": 10, "y": 20}],
            "status": "completed",
        });
        let mut request = native_computer_followup("gpt-5.4", call);
        let first_result = request
            .messages
            .iter_mut()
            .flat_map(|message| &mut message.blocks)
            .find_map(|block| match block {
                Block::ToolResult { preview, .. } => Some(preview),
                _ => None,
            })
            .expect("synthetic action result");
        *first_result = serde_json::json!({
            "status": status,
            "error": {"kind": "test_failure"},
        })
        .to_string();

        let error = responses_request_json(&request, false, None, false)
            .expect_err("an incomplete native action must not become computer_call_output");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(error.message.contains("did not complete"), "{error:?}");
        assert!(error.message.contains(status), "{error:?}");
    }

    let call = serde_json::json!({
        "type": "computer_call",
        "call_id": "missing_intermediate",
        "action": {"type": "click", "button": "left", "x": 10, "y": 20},
        "pending_safety_checks": [{"id": "must_not_ack"}],
        "status": "completed",
    });
    let mut request = native_computer_followup("computer-use-preview", call);
    let missing_call_id = native_computer_action_call_id("missing_intermediate", 0);
    request.messages.retain(|message| {
        !message.blocks.iter().any(
            |block| matches!(block, Block::ToolResult { call_id, .. } if call_id == &missing_call_id),
        )
    });
    let error = responses_request_json(&request, false, None, false)
        .expect_err("the final screenshot cannot cover a missing action result");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("missing result 1 of 2"), "{error:?}");
}

#[test]
fn user_command_record_reaches_both_openai_wires_as_labeled_user_text() {
    let mut request = probe_request("gpt-5.6-sol");
    request.messages = vec![Message::user_command(UserCommandRecord {
        call_id: "user-command-openai".into(),
        command: "printf openai-user-command".into(),
        status: ToolStatus::Completed,
        exit_code: Some(0),
        output_preview: "[stdout]\nopenai-user-command".into(),
        output_bytes: 19,
        output_truncated: false,
        output_lossy_utf8: false,
    })];

    let responses = responses_request_json(&request, false, None, false)
        .expect("OpenAI Responses user-command payload");
    let responses_user = responses["input"]
        .as_array()
        .expect("Responses input")
        .iter()
        .find(|entry| entry["role"] == "user")
        .expect("Responses user entry");
    let responses_text = responses_user["content"][0]["text"]
        .as_str()
        .expect("Responses text block");
    assert_user_command_wire_text(responses_text, "openai-user-command");

    let chat = chat_request_json(&request, CompatibleDialect::Generic, None, None)
        .expect("OpenAI-compatible user-command payload");
    let chat_user = chat["messages"]
        .as_array()
        .expect("chat messages")
        .iter()
        .find(|entry| entry["role"] == "user")
        .expect("chat user entry");
    let chat_text = chat_user["content"][0]["text"]
        .as_str()
        .expect("chat text content");
    assert_user_command_wire_text(chat_text, "openai-user-command");
}

fn assert_user_command_wire_text(text: &str, output: &str) {
    assert!(text.contains("[user-initiated shell command]"));
    assert!(text.contains("origin: user_command"));
    assert!(text.contains("printf openai-user-command"));
    assert!(text.contains(output));
}

fn cm2_cache_metadata(provider: &str, stable_history_end: usize) -> PromptCacheMetadata {
    PromptCacheMetadata {
        stable_history_end,
        cacheable_history_end: None,
        current_user_start: stable_history_end,
        previous_stable_history_end: None,
        latest_compaction_summary_end: Some(1),
        prefix_digests: PrefixDigests {
            system: "system-a".into(),
            tools: "tools-a".into(),
            immutable_history: "history-a".into(),
            model: "model-a".into(),
            auth_mode: "auth-a".into(),
            reasoning_settings: "reasoning-a".into(),
        },
        cache_epoch: "epoch-a".into(),
        header_epoch: "provider-header-a".into(),
        compaction_epoch: "compaction-a".into(),
        provider: provider.into(),
        session_scope: "session-a".into(),
        cache_cohort: None,
        account_scope: Some("account-a".into()),
        stable_prefix_tokens: 8_192,
        expected_later_reads: 2,
        reuse_gap_ms: Some(10_000),
    }
}

fn prepared_cohort_key(provider: &dyn crate::Provider, request: &TurnRequest) -> String {
    let prepared = provider
        .prepare_turn(request)
        .expect("prepared provider turn");
    let mut finalized = request.clone();
    finalized
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .prefix_digests = prepared.prefix_digests().clone();
    finalized
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .header_epoch = prepared
        .provider_view()
        .expect("prepared provider view")
        .ledger()
        .header_epoch
        .clone();
    prompt_cache_cohort_key(
        &finalized,
        finalized.cache_metadata.as_ref().expect("cache metadata"),
    )
    .expect("prepared cohort key")
}

fn remove_openai_cache_metadata(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            values.iter_mut().for_each(remove_openai_cache_metadata);
        }
        serde_json::Value::Object(values) => {
            values.remove("prompt_cache_key");
            values.remove("prompt_cache_options");
            values.remove("prompt_cache_breakpoint");
            values.values_mut().for_each(remove_openai_cache_metadata);
        }
        _ => {}
    }
}

#[test]
fn cache_diagnostic_openai_hashes_current_wire_through_previous_history_length() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("openai-cache-diagnostic");
    vault
        .put(&alias, b"openai-cache-diagnostic-key")
        .expect("store test credential");
    let provider = OpenAiProvider::new(
        vault.resolve(&alias).expect("resolve test credential"),
        "gpt-5.6",
    )
    .expect("construct OpenAI provider");
    let mut first = probe_request("gpt-5.6");
    first.messages.push(Message::assistant(vec![Block::Text {
        text: "first answer".into(),
    }]));
    first.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    let first_prepared = crate::Provider::prepare_turn(&provider, &first).expect("first prepared");

    let mut grown = first;
    grown.messages.push(Message::user_text("next question"));
    let metadata = grown.cache_metadata.as_mut().expect("metadata");
    metadata.previous_stable_history_end = Some(1);
    metadata.stable_history_end = 2;
    metadata.current_user_start = 2;
    let grown_prepared = crate::Provider::prepare_turn(&provider, &grown).expect("grown prepared");

    assert_eq!(
        grown_prepared.previous_immutable_history_digest(),
        Some(first_prepared.prefix_digests().immutable_history.as_str()),
        "the old Responses wire prefix remains hashable after history grows"
    );
    assert_ne!(
        grown_prepared.prefix_digests().immutable_history,
        first_prepared.prefix_digests().immutable_history
    );
}

/// The actor learns `header_epoch` from the prepared provider view. Preparation
/// must nevertheless classify the exact wire it built with that finalized
/// header, rather than reporting a missing key until the later send refresh.
#[test]
fn prepared_openai_cache_control_uses_the_provider_view_header_epoch() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("openai-finalized-header");
    vault
        .put(&alias, b"openai-finalized-header-key")
        .expect("store test credential");
    let provider = OpenAiProvider::new(
        vault.resolve(&alias).expect("resolve test credential"),
        "gpt-5.6",
    )
    .expect("construct OpenAI provider");
    let mut request = probe_request("gpt-5.6");
    request.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    request
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .header_epoch
        .clear();

    let prepared = crate::Provider::prepare_turn(&provider, &request).expect("prepared turn");
    assert!(
        !prepared
            .provider_view()
            .expect("provider view")
            .ledger()
            .header_epoch
            .is_empty()
    );
    let payload = &prepared.wire.as_ref().expect("prepared wire").payload;
    assert!(payload.get("prompt_cache_key").is_some());
    assert_eq!(
        payload.get("prompt_cache_options"),
        Some(&serde_json::json!({"mode": "explicit", "ttl": "30m"})),
        "the finalized cohort key and explicit TTL must enter the same prepared wire"
    );
    assert_eq!(
        *prepared.cache_control(),
        haider_protocol::provider::CacheControlObservationV1::Emitted {
            ttl_ms: Some(30 * 60 * 1_000),
        }
    );
}

#[test]
fn prepared_openai_and_compatible_wire_bytes_match_legacy_final_render() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("openai-single-render-wire");
    vault
        .put(&alias, b"openai-single-render-wire-key")
        .expect("store test credential");
    let openai = OpenAiProvider::new(
        vault.resolve(&alias).expect("resolve test credential"),
        "gpt-5.6",
    )
    .expect("construct OpenAI provider");
    let mut request = probe_request("gpt-5.6");
    request.tools = vec![computer_tool_definition()];
    request.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    let prepared = crate::Provider::prepare_turn(&openai, &request).expect("prepared OpenAI turn");
    let mut borrowed_request = request.clone();
    let shared_tools = std::mem::take(&mut borrowed_request.tools);
    let borrowed =
        crate::Provider::prepare_turn_with_tools(&openai, &borrowed_request, &shared_tools)
            .expect("borrowed-tools prepared OpenAI turn");
    assert_eq!(
        serde_json::to_vec(&borrowed.wire.as_ref().expect("borrowed wire").payload)
            .expect("borrowed OpenAI bytes"),
        serde_json::to_vec(&prepared.wire.as_ref().expect("prepared wire").payload)
            .expect("prepared OpenAI bytes"),
        "Arc-backed preparation must preserve exact OpenAI wire bytes"
    );
    let mut finalized = request.clone();
    finalized
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .header_epoch = prepared
        .provider_view()
        .expect("provider view")
        .ledger()
        .header_epoch
        .clone();
    let legacy = openai
        .request_payload(&finalized)
        .expect("legacy OpenAI payload");
    assert_eq!(
        serde_json::to_vec(&prepared.wire.as_ref().expect("prepared wire").payload)
            .expect("prepared OpenAI bytes"),
        serde_json::to_vec(&legacy).expect("legacy OpenAI bytes")
    );

    let compatible = compatible_provider_with_resolver(
        b"compatible-single-render-wire-key",
        "https://compatible-wire.example",
        Arc::new(StubDnsResolver::new(std::iter::empty::<Vec<SocketAddr>>())),
    );
    let mut compatible_request = probe_request("audit-model");
    compatible_request.tools = vec![computer_tool_definition()];
    compatible_request.cache_metadata =
        Some(cm2_cache_metadata(OPENAI_COMPATIBLE_PROVIDER_NAME, 1));
    let compatible_prepared = crate::Provider::prepare_turn(&compatible, &compatible_request)
        .expect("prepared compatible turn");
    let mut borrowed_compatible_request = compatible_request.clone();
    let shared_compatible_tools = std::mem::take(&mut borrowed_compatible_request.tools);
    let borrowed_compatible = crate::Provider::prepare_turn_with_tools(
        &compatible,
        &borrowed_compatible_request,
        &shared_compatible_tools,
    )
    .expect("borrowed-tools prepared compatible turn");
    assert_eq!(
        serde_json::to_vec(
            &borrowed_compatible
                .wire
                .as_ref()
                .expect("borrowed compatible wire")
                .payload,
        )
        .expect("borrowed compatible bytes"),
        serde_json::to_vec(
            &compatible_prepared
                .wire
                .as_ref()
                .expect("prepared compatible wire")
                .payload,
        )
        .expect("prepared compatible bytes"),
        "Arc-backed preparation must preserve exact compatible wire bytes"
    );
    let compatible_legacy = compatible
        .request_payload(&compatible_request)
        .expect("legacy compatible payload");
    assert_eq!(
        serde_json::to_vec(
            &compatible_prepared
                .wire
                .as_ref()
                .expect("prepared compatible wire")
                .payload
        )
        .expect("prepared compatible bytes"),
        serde_json::to_vec(&compatible_legacy).expect("legacy compatible bytes")
    );
}

/// CM2d — the routing key identifies one account/model/header/fork cohort.
/// Rendered prefix diagnostics do not rotate it; the provider-view header
/// epoch is the one authoritative address of the actual stable base.
///
/// MUTATION CHECK: put history/compaction into the key or replace the header
/// epoch with a second system/tool digest path; an assertion fails.
#[test]
fn cm2d_openai_prompt_cache_key_is_stable_and_domain_sensitive() {
    let mut request = probe_request("gpt-5.6");
    request.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    request
        .cache_metadata
        .as_mut()
        .expect("metadata")
        .header_epoch = "provider-header-a".into();
    let metadata = request.cache_metadata.as_ref().expect("metadata");
    let first = prompt_cache_cohort_key(&request, metadata).expect("cohort key");

    for mutate in ["history", "compaction"] {
        let mut changed = request.cache_metadata.clone().expect("metadata");
        match mutate {
            "history" => changed
                .prefix_digests
                .immutable_history
                .push_str("-changed"),
            "compaction" => changed.compaction_epoch.push_str("-changed"),
            _ => unreachable!(),
        }
        assert_eq!(
            first,
            prompt_cache_cohort_key(&request, &changed).expect("unchanged cohort key"),
            "{mutate} is prefix-match state, not routing identity"
        );
    }

    for mutate in ["system-diagnostic", "tools-diagnostic"] {
        let mut changed = request.cache_metadata.clone().expect("metadata");
        match mutate {
            "system-diagnostic" => changed.prefix_digests.system.push_str("-changed"),
            "tools-diagnostic" => changed.prefix_digests.tools.push_str("-changed"),
            _ => unreachable!(),
        }
        assert_eq!(
            first,
            prompt_cache_cohort_key(&request, &changed).expect("same finalized header key"),
            "{mutate} cannot compete with the finalized provider-view header"
        );
    }

    let mut other_account = request.cache_metadata.clone().expect("metadata");
    other_account.account_scope = Some("account-b".into());
    assert_ne!(
        first,
        prompt_cache_cohort_key(&request, &other_account).expect("other account key")
    );

    let mut other_header = request.cache_metadata.clone().expect("metadata");
    other_header.header_epoch = "provider-header-b".into();
    assert_ne!(
        first,
        prompt_cache_cohort_key(&request, &other_header).expect("other header key"),
        "stable-header ABI changes must select a new cache route"
    );
}

/// HAIDER963 Q5. MUTATION CHECK: keep the generic/custom dialect outside the
/// v3 cohort overlay, accept a builtin provider name, or drop the account
/// scope. The presence/absence assertions below fail in each direction.
#[test]
fn custom_openai_compatible_uses_v3_prompt_cache_cohort() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("router-lab");
    vault
        .put(&alias, b"CUSTOM_CACHE_SENTINEL_963")
        .expect("store custom key");
    let provider = OpenAiCompatibleProvider::new_custom(
        vault.resolve(&alias).expect("resolve custom key"),
        "router-model",
        "http://127.0.0.1:11434/v1",
    )
    .expect("custom provider")
    .with_account(alias);
    let mut request = probe_request("router-model");
    request.cache_metadata = Some(cm2_cache_metadata("router-lab", 1));
    request
        .cache_metadata
        .as_mut()
        .expect("custom metadata")
        .account_scope = Some("router-lab".into());

    let routed = provider.request_payload(&request).expect("custom payload");
    assert!(routed.get("prompt_cache_key").is_some());

    request
        .cache_metadata
        .as_mut()
        .expect("custom metadata")
        .account_scope = None;
    let unscoped = provider
        .request_payload(&request)
        .expect("unscoped payload");
    assert!(unscoped.get("prompt_cache_key").is_none());

    request
        .cache_metadata
        .as_mut()
        .expect("custom metadata")
        .account_scope = Some("router-lab".into());
    request
        .cache_metadata
        .as_mut()
        .expect("custom metadata")
        .provider = OPENAI_PROVIDER_NAME.into();
    let builtin = provider
        .request_payload(&request)
        .expect("mismatched payload");
    assert!(builtin.get("prompt_cache_key").is_none());
}

/// CACHE ITEM 962. Provider adapters place the shared system base in three
/// different wire locations. Every routed path must hash its rendered value,
/// including the opaque grant-scope marker, plus its finalized hosted schemas.
///
/// MUTATION CHECK: hash absent top-level `instructions`/`system` keys on the
/// Responses-lite or Chat paths, or ignore hosted tools; an assertion fails.
#[test]
fn routed_adapters_finalize_their_rendered_system_base_into_the_cohort() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("rendered-system-cohort");
    vault
        .put(&alias, b"rendered-system-cohort-secret")
        .expect("store test credential");
    let providers: Vec<(&str, Box<dyn crate::Provider>, &str, &str)> = vec![
        (
            "OpenAI Responses-lite",
            Box::new(
                OpenAiProvider::new_subscription(
                    vault.resolve(&alias).expect("resolve OpenAI credential"),
                    "gpt-5.6",
                    OPENAI_SUBSCRIPTION_BASE_URL,
                )
                .expect("construct OpenAI subscription adapter"),
            ),
            OPENAI_OAUTH_PROVIDER_NAME,
            "gpt-5.6",
        ),
        (
            "Kimi Chat Completions",
            Box::new(
                OpenAiCompatibleProvider::new_kimi_subscription(
                    vault.resolve(&alias).expect("resolve Kimi credential"),
                    "kimi-coding-a",
                    KIMI_OAUTH_BASE_URL,
                )
                .expect("construct Kimi adapter"),
            ),
            KIMI_OAUTH_PROVIDER_NAME,
            "kimi-coding-a",
        ),
        (
            "xAI Chat Completions",
            Box::new(
                OpenAiCompatibleProvider::new_xai_api(
                    vault.resolve(&alias).expect("resolve xAI credential"),
                    "grok-4.6",
                    XAI_BASE_URL,
                )
                .expect("construct xAI adapter"),
            ),
            XAI_PROVIDER_NAME,
            "grok-4.6",
        ),
    ];

    for (label, provider, provider_name, model) in providers {
        let mut first = probe_request(model);
        first.system_prompt = Some("shared base; grant-scope=a".into());
        first.cache_metadata = Some(cm2_cache_metadata(provider_name, 1));
        let first_key = prepared_cohort_key(provider.as_ref(), &first);

        let mut other_grant = first;
        other_grant.system_prompt = Some("shared base; grant-scope=b".into());
        let other_key = prepared_cohort_key(provider.as_ref(), &other_grant);
        assert_ne!(
            first_key, other_key,
            "{label} must isolate different rendered system/grant bases"
        );
    }

    let plain = OpenAiProvider::new(
        vault.resolve(&alias).expect("resolve plain credential"),
        "gpt-5.6",
    )
    .expect("construct plain OpenAI adapter");
    let hosted = OpenAiProvider::new(
        vault.resolve(&alias).expect("resolve hosted credential"),
        "gpt-5.6",
    )
    .expect("construct hosted OpenAI adapter")
    .with_web_search(true);
    let mut request = probe_request("gpt-5.6");
    request.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    assert_ne!(
        prepared_cohort_key(&plain, &request),
        prepared_cohort_key(&hosted, &request),
        "provider-added hosted schemas must participate in the finalized base"
    );
}

/// HAIDER963(a-c). MUTATION CHECK: omit the session fallback, trust every
/// same-account session, or ignore an inherited C3 root. The unrelated/fresh
/// inequalities or inherited-fork equality fail.
#[test]
fn openai_prompt_cache_key_isolates_sessions_and_shares_only_an_inherited_fork_root() {
    let mut parent = probe_request("gpt-5.6");
    parent.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    let parent_key = openai_prompt_cache_key(&parent).expect("parent session key");

    let mut child = parent.clone();
    child
        .cache_metadata
        .as_mut()
        .expect("child metadata")
        .session_scope = "session-b".into();
    let fresh_key = openai_prompt_cache_key(&child).expect("fresh child key");
    assert_ne!(
        parent_key, fresh_key,
        "unrelated sessions and fresh-epoch forks must remain isolated"
    );

    child
        .cache_metadata
        .as_mut()
        .expect("child metadata")
        .cache_cohort = Some("session-a".into());
    let inherited_key = openai_prompt_cache_key(&child).expect("inherited child key");
    assert_eq!(
        parent_key, inherited_key,
        "a byte-identical inherited fork must share its durable root cohort"
    );
}

/// HAIDER949(b). MUTATION CHECK: add `request.messages.len()` to the cache-key
/// domain; the equality assertion below reports that append-only turn growth
/// changed a session's key.
#[test]
fn openai_prompt_cache_key_is_stable_across_turns_in_one_session() {
    let mut first_turn = probe_request("gpt-5.6");
    first_turn.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    let first = openai_prompt_cache_key(&first_turn).expect("first turn key");

    let mut second_turn = first_turn.clone();
    second_turn
        .messages
        .push(Message::assistant(vec![Block::Text {
            text: "append-only answer".into(),
        }]));
    second_turn
        .messages
        .push(Message::user_text("next turn in the same session"));
    let second_stable_history_end = second_turn.messages.len() - 1;
    let metadata = second_turn
        .cache_metadata
        .as_mut()
        .expect("second metadata");
    metadata.previous_stable_history_end = Some(metadata.stable_history_end);
    metadata.stable_history_end = second_stable_history_end;
    metadata.current_user_start = second_stable_history_end;
    metadata.prefix_digests.immutable_history = "history-b".into();
    metadata.compaction_epoch = "compaction-b".into();
    let second = openai_prompt_cache_key(&second_turn).expect("second turn key");

    assert_eq!(
        first, second,
        "append-only turns in one session must retain the OpenAI cache key"
    );
}

fn openai_model_prefix_bytes(payload: &serde_json::Value, input_end: usize) -> Vec<u8> {
    let input = payload["input"].as_array().expect("Responses input");
    serde_json::to_vec(&serde_json::json!({
        "instructions": payload.get("instructions"),
        "tools": payload.get("tools"),
        "immutable_history": &input[..input_end],
    }))
    .expect("serialize OpenAI model prefix")
}

/// The model-visible prefix is append-only on both Responses dialects. Cache
/// controls are stripped because they select/write a prefix without becoming
/// model input; every system, tool, and old-history byte must still match.
#[test]
fn openai_rendered_prefix_bytes_are_stable_across_turns() {
    for codex_responses_lite in [false, true] {
        let mut first_turn = TurnRequest {
            messages: vec![
                Message::user_text("stable question"),
                Message::assistant(vec![Block::Text {
                    text: "stable answer".into(),
                }]),
                Message::user_text("current question"),
            ],
            model: "gpt-5.6".into(),
            max_tokens: 256,
            system_prompt: Some("stable system".into()),
            tools: vec![ToolDefinition {
                name: "lookup".into(),
                description: "stable tool".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"],
                }),
            }],
            attachments: Vec::new(),
            cache_metadata: Some(cm2_cache_metadata(
                if codex_responses_lite {
                    OPENAI_OAUTH_PROVIDER_NAME
                } else {
                    OPENAI_PROVIDER_NAME
                },
                2,
            )),
        };
        let (mut first_payload, first_wire_end, _) =
            responses_request_json_with_boundary(&first_turn, codex_responses_lite, None, false, 2)
                .expect("first OpenAI wire");

        let mut second_turn = first_turn.clone();
        second_turn
            .messages
            .push(Message::assistant(vec![Block::Text {
                text: "current answer".into(),
            }]));
        second_turn
            .messages
            .push(Message::user_text("next question"));
        let metadata = second_turn.cache_metadata.as_mut().expect("metadata");
        metadata.previous_stable_history_end = Some(2);
        metadata.stable_history_end = 4;
        metadata.current_user_start = 4;
        metadata.prefix_digests.immutable_history = "grown-history".into();
        let (mut second_payload, _, previous_wire_end) = responses_request_json_with_boundary(
            &second_turn,
            codex_responses_lite,
            None,
            false,
            4,
        )
        .expect("second OpenAI wire");

        assert_eq!(
            first_payload["prompt_cache_key"], second_payload["prompt_cache_key"],
            "one session keeps its routing key on the lite={codex_responses_lite} path"
        );
        remove_openai_cache_metadata(&mut first_payload);
        remove_openai_cache_metadata(&mut second_payload);
        assert_eq!(
            openai_model_prefix_bytes(&first_payload, first_wire_end),
            openai_model_prefix_bytes(
                &second_payload,
                previous_wire_end.expect("previous wire boundary"),
            ),
            "system, tools, and immutable history stay byte-identical on the lite={codex_responses_lite} path"
        );

        first_turn
            .cache_metadata
            .as_mut()
            .expect("metadata")
            .session_scope = "session-b".into();
        first_turn
            .cache_metadata
            .as_mut()
            .expect("metadata")
            .cache_cohort = Some("session-a".into());
        assert_eq!(
            openai_prompt_cache_key(&first_turn),
            openai_prompt_cache_key(&second_turn),
            "an inherited fork with the same bytes must use its root cohort route"
        );
    }
}

/// Account scope is the hard tenant boundary. Without it the adapter must
/// fail closed instead of placing unrelated anonymous callers in one cohort.
#[test]
fn openai_prompt_cache_key_is_omitted_without_account_scope() {
    let mut request = probe_request("gpt-5.6");
    request.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    for missing in [None, Some(String::new())] {
        request
            .cache_metadata
            .as_mut()
            .expect("metadata")
            .account_scope = missing;
        assert_eq!(
            openai_prompt_cache_key(&request),
            None,
            "requests without a trusted account cannot provide cache routing"
        );
        let payload = responses_request_json(&request, false, None, false).expect("OpenAI wire");
        assert!(
            !payload.to_string().contains("prompt_cache"),
            "no cache control may be emitted without account isolation: {payload}"
        );
    }
}

/// GPT-5.6 gets explicit breakpoints plus the stable cache key. C1's frozen
/// snapshot is part of the immutable prefix, while the current user remains
/// deliberately beyond the final marker.
#[test]
fn cm2d_gpt56_uses_explicit_breakpoints_before_the_volatile_suffix() {
    let mut request = TurnRequest {
        messages: vec![
            Message::user_text("summary"),
            Message::user_text("stable history"),
            Message::user_text("frozen daemon context"),
            Message::user_text("volatile current turn"),
        ],
        model: "gpt-5.6".into(),
        max_tokens: 256,
        system_prompt: Some("stable system".into()),
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 3)),
    };
    request
        .cache_metadata
        .as_mut()
        .expect("metadata")
        .current_user_start = 3;
    let payload = responses_request_json(&request, false, None, false).expect("GPT-5.6 wire");
    assert!(payload.get("prompt_cache_key").is_some());
    assert_eq!(
        payload["prompt_cache_options"],
        serde_json::json!({"mode": "explicit", "ttl": "30m"})
    );
    assert_eq!(
        payload["input"][0]["content"][0]["prompt_cache_breakpoint"],
        serde_json::json!({"mode": "explicit"})
    );
    assert_eq!(
        payload["input"][2]["content"][0]["prompt_cache_breakpoint"],
        serde_json::json!({"mode": "explicit"})
    );
    assert!(
        payload["input"][1]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none(),
        "the middle stable block does not consume the second marker: {payload}"
    );
    assert!(
        payload["input"][3]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none(),
        "volatile suffix must not be explicitly written: {payload}"
    );
}

/// HAIDER953. The public Responses API spells an explicit content marker
/// `prompt_cache_breakpoint: {"mode":"explicit"}`, but the 947 law forbids
/// treating that surface as evidence for HTTPS responses-lite. Exercise the
/// environment split in isolated child processes so parallel tests cannot
/// observe a process-global environment mutation.
///
/// MUTATION CHECK: delete the environment-gate check (making the marker
/// unconditional), or make the gate inert; the default-off or enabled child
/// respectively fails. The API-key assertion also pins transport isolation.
#[test]
fn haider953_openai_lite_cache_breakpoint_is_explicitly_gated() {
    const CHILD_MARKER: &str = "HAIDER_OPENAI_LITE_CACHE_BREAKPOINT_TEST_CHILD";
    const EXPECTED_MARKER: &str = "HAIDER_OPENAI_LITE_CACHE_BREAKPOINT_TEST_EXPECTED";

    if std::env::var_os(CHILD_MARKER).is_some() {
        let expected = std::env::var_os(EXPECTED_MARKER).is_some();
        let mut lite_request = probe_request("gpt-5.6-sol");
        lite_request.cache_metadata = Some(cm2_cache_metadata(OPENAI_OAUTH_PROVIDER_NAME, 1));
        let lite = responses_request_json(&lite_request, true, None, false).expect("lite wire");
        let marker = lite["input"][0]["content"][0]
            .get("prompt_cache_breakpoint")
            .cloned();
        assert_eq!(
            marker,
            expected.then(|| serde_json::json!({"mode": "explicit"})),
            "the HTTPS lite marker must exactly follow the default-off environment gate: {lite}"
        );

        let api_key = responses_request_json(&probe_request("gpt-5.6-sol"), false, None, false)
            .expect("API-key wire");
        assert!(
            api_key["input"][0]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none(),
            "the lite experiment gate must not annotate public API-key requests: {api_key}"
        );
        return;
    }

    for enabled in [false, true] {
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
        child
            .arg("haider953_openai_lite_cache_breakpoint_is_explicitly_gated")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .env_remove(OPENAI_LITE_CACHE_BREAKPOINT_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if enabled {
            child
                .env(OPENAI_LITE_CACHE_BREAKPOINT_ENV, "1")
                .env(EXPECTED_MARKER, "1");
        } else {
            child.env_remove(EXPECTED_MARKER);
        }
        let output = child.output().expect("run isolated breakpoint child");
        assert!(
            output.status.success(),
            "breakpoint child (enabled={enabled}) failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// CM2f — the HTTPS responses-lite transport rejects
/// `prompt_cache_retention`, even though it appeared in a WebSocket codex
/// capture. Haider keeps its stable cache key and puts the system prompt in
/// the leading developer input item with top-level `instructions` null.
/// Stripping the cache key restores the CM1 byte-exact request, so it never
/// changes model-visible content.
///
/// MUTATION CHECK: restore top-level lite `instructions`, remove the leading
/// developer item, add HTTPS retention, or suppress OAuth key derivation; the
/// corresponding shape assertion fails.
#[test]
fn cm2f_openai_lite_moves_instructions_to_input_and_omits_https_retention() {
    let mut request = probe_request("gpt-5.6-sol");
    request.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    let with_metadata = responses_request_json(&request, true, None, false).expect("lite wire");
    let again = responses_request_json(&request, true, None, false).expect("lite wire again");
    let key = with_metadata
        .get("prompt_cache_key")
        .and_then(serde_json::Value::as_str)
        .expect("lite request carries prompt_cache_key");
    assert!(!key.is_empty());
    assert_eq!(
        again
            .get("prompt_cache_key")
            .and_then(serde_json::Value::as_str),
        Some(key),
        "the cache key must be stable across identical requests"
    );
    assert!(
        with_metadata.get("prompt_cache_retention").is_none(),
        "HTTPS lite rejects prompt_cache_retention: {with_metadata}"
    );
    assert!(
        with_metadata.get("instructions").is_none(),
        "lite leaves the top-level instructions parameter null/absent: {with_metadata}"
    );
    assert_eq!(
        with_metadata["input"][0],
        serde_json::json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": "Be terse."}],
        }),
        "lite instructions are the leading developer input item: {with_metadata}"
    );
    // Content invariance: strip the cache annotations and the CM1 unannotated
    // request comes back byte-exact.
    let mut stripped = with_metadata.clone();
    let object = stripped.as_object_mut().expect("request object");
    object.remove("prompt_cache_key");
    request.cache_metadata = None;
    let baseline = responses_request_json(&request, true, None, false).expect("CM1 wire");
    assert_eq!(stripped, baseline);
    assert!(!stripped.to_string().contains("prompt_cache"));
}

/// OAuth and API-key sessions both receive stable cache routing, while the
/// provider name keeps their cache domains separate. OAuth omits the field
/// rejected by the HTTPS lite transport and reports no explicit TTL.
#[test]
fn cm2f_openai_oauth_omits_https_retention_and_is_separate_from_api_key() {
    let mut oauth_request = probe_request("gpt-5.6-sol");
    oauth_request.cache_metadata = Some(cm2_cache_metadata(OPENAI_OAUTH_PROVIDER_NAME, 1));
    let oauth_payload =
        responses_request_json(&oauth_request, true, None, false).expect("OAuth wire");
    let oauth_key = oauth_payload
        .get("prompt_cache_key")
        .and_then(serde_json::Value::as_str)
        .expect("OAuth request carries prompt_cache_key");
    assert!(
        oauth_payload.get("prompt_cache_retention").is_none(),
        "OAuth/HTTPS-lite request omits rejected retention: {oauth_payload}"
    );
    assert_eq!(
        openai_cache_control_observation(&oauth_request, &oauth_payload),
        haider_protocol::provider::CacheControlObservationV1::Emitted { ttl_ms: None },
        "cache telemetry reports no TTL when the wire emits none"
    );

    let mut api_key_request = probe_request("gpt-5.6-sol");
    api_key_request.cache_metadata = Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 1));
    let api_key = openai_prompt_cache_key(&api_key_request)
        .expect("API-key request carries prompt_cache_key");
    assert_ne!(
        oauth_key, api_key,
        "provider identity must separate OAuth and API-key cache domains"
    );
}

/// CM2g — strip only the new cache keys and the GPT-5.6 request becomes the
/// byte-equivalent unannotated request, including provider-opaque reasoning
/// and provider-produced tool arguments in their original order.
///
/// MUTATION CHECK (executed): normalize/reorder the opaque object or tool
/// arguments, or omit a message while adding markers; equality fails.
#[test]
fn cm2g_openai_cache_keys_do_not_change_model_visible_content() {
    let opaque = serde_json::json!({
        "type": "reasoning",
        "id": "reasoning-1",
        "encrypted_content": "signed-provider-bytes"
    });
    let args = serde_json::json!({"z": 1, "a": {"second": 2, "first": 1}});
    let mut request = TurnRequest {
        messages: vec![
            Message::user_text("summary"),
            Message::assistant(vec![
                Block::ProviderOpaque {
                    provider: OPENAI_PROVIDER_NAME.into(),
                    data: opaque,
                },
                Block::ToolCall {
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    args,
                },
            ]),
            Message::user_text("current"),
        ],
        model: "gpt-5.6".into(),
        max_tokens: 256,
        system_prompt: Some("system".into()),
        tools: vec![ToolDefinition {
            name: "lookup".into(),
            description: "lookup".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        attachments: Vec::new(),
        cache_metadata: Some(cm2_cache_metadata(OPENAI_PROVIDER_NAME, 2)),
    };
    let mut annotated =
        responses_request_json(&request, false, None, false).expect("annotated wire");
    remove_openai_cache_metadata(&mut annotated);
    request.cache_metadata = None;
    let baseline = responses_request_json(&request, false, None, false).expect("baseline wire");
    assert_eq!(annotated, baseline);
}

#[test]
fn cm2g_kimi_cache_key_preserves_thinking_tools_and_arguments() {
    let mut request = TurnRequest {
        messages: vec![
            Message::user_text("stable"),
            Message::assistant(vec![Block::ToolCall {
                call_id: "call-kimi".into(),
                name: "lookup".into(),
                args: serde_json::json!({"z": 1, "a": {"second": 2, "first": 1}}),
            }]),
            Message::tool_result("call-kimi", "exact result", false),
        ],
        model: "kimi-coding-a".into(),
        max_tokens: 256,
        system_prompt: Some("stable system".into()),
        tools: vec![ToolDefinition {
            name: "lookup".into(),
            description: "exact tool".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"z": {"type": "number"}, "a": {"type": "object"}}
            }),
        }],
        attachments: Vec::new(),
        cache_metadata: Some(cm2_cache_metadata(KIMI_OAUTH_PROVIDER_NAME, 2)),
    };
    let thinking = KimiThinkingConfig {
        thinking_type: KimiThinkingType::Enabled,
        effort: Some("high".into()),
        keep: Some("all".into()),
    };
    let mut annotated = chat_request_json(
        &request,
        CompatibleDialect::KimiOAuth,
        Some(&thinking),
        Some("high"),
    )
    .expect("annotated Kimi wire");
    annotated
        .as_object_mut()
        .expect("Kimi object")
        .remove("prompt_cache_key");
    request.cache_metadata = None;
    let baseline = chat_request_json(
        &request,
        CompatibleDialect::KimiOAuth,
        Some(&thinking),
        Some("high"),
    )
    .expect("baseline Kimi wire");
    assert_eq!(annotated, baseline);
}

#[test]
fn cm2f_unknown_compatible_endpoint_is_byte_exact_without_annotations() {
    let mut request = probe_request("unknown-local-model");
    request.cache_metadata = Some(cm2_cache_metadata(OPENAI_COMPATIBLE_PROVIDER_NAME, 1));
    let with_metadata = chat_request_json(&request, CompatibleDialect::Generic, None, None)
        .expect("unknown compatible fallback");
    request.cache_metadata = None;
    let baseline = chat_request_json(&request, CompatibleDialect::Generic, None, None)
        .expect("CM1 compatible wire");
    assert_eq!(with_metadata, baseline);
    assert!(!with_metadata.to_string().contains("prompt_cache"));
}

/// MUTATION CHECK (W5f-2d): in `responses_request_json`, keep inserting
/// `max_output_tokens` under lite, or drop the `parallel_tool_calls=false` /
/// `reasoning.context=all_turns` inserts. Expected runtime failure: the
/// assertions below fail — and against the LIVE codex endpoint each of those
/// three is a hard 400 (confirmed 2026-07-30: "Unsupported parameter:
/// max_output_tokens", "requires parallel_tool_calls to be false", "requires
/// reasoning.context to be all_turns").
/// Verified by revert on 2026-07-30.
#[test]
fn codex_lite_payload_meets_the_subscription_contract() {
    // W-B (LW4): the hosted-web-search flag is DELIBERATELY set here — the
    // lite contract golden now also pins that lite NEVER carries hosted
    // tools, even when the pair-level flag asks for them.
    let payload = responses_request_json(&probe_request("gpt-5.6-sol"), true, None, true)
        .expect("lite payload");
    let object = payload.as_object().expect("object");
    assert!(
        !object.contains_key("max_output_tokens"),
        "lite REJECTS max_output_tokens: {payload}"
    );
    assert_eq!(
        object.get("parallel_tool_calls"),
        Some(&serde_json::Value::Bool(false)),
        "lite requires parallel_tool_calls=false"
    );
    assert_eq!(
        payload
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("context"))
            .and_then(|context| context.as_str()),
        Some("all_turns"),
        "lite requires reasoning.context=all_turns: {payload}"
    );
    assert!(
        !object.contains_key("instructions"),
        "codex leaves top-level instructions null and uses developer input: {payload}"
    );
    assert_eq!(
        payload["input"][0],
        serde_json::json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": "Be terse."}],
        }),
        "lite instructions must be the leading developer input item: {payload}"
    );
    assert!(
        !object.contains_key("tools"),
        "lite REJECTS hosted tools — the web_search flag must be structurally inert: {payload}"
    );
}

/// LAW (LW2 openai half / LW4): the hosted `{\"type\":\"web_search\"}` tool
/// with `search_context_size: medium` declares on the API-KEY Responses path
/// when the pair-level flag is set — exactly that shape, appended after any
/// client function tools — and NEVER on lite (structural, asserted in the
/// lite contract golden above). Without the flag the API-key body is
/// byte-identical to the pre-W-B shape.
#[test]
fn hosted_web_search_declares_on_api_key_and_never_on_lite() {
    let hosted = responses_request_json(&probe_request("gpt-5.6-sol"), false, None, true)
        .expect("api-key hosted payload");
    assert_eq!(
        hosted.get("tools"),
        Some(&serde_json::json!([
            {"type": "web_search", "search_context_size": "medium"}
        ])),
        "the hosted tool declares with exactly this shape: {hosted}"
    );

    let without_flag = responses_request_json(&probe_request("gpt-5.6-sol"), false, None, false)
        .expect("api-key payload");
    assert!(
        without_flag.get("tools").is_none(),
        "no hosted tool without the flag"
    );

    let lite = responses_request_json(&probe_request("gpt-5.6-sol"), true, None, true)
        .expect("lite payload");
    assert!(
        lite.get("tools").is_none(),
        "lite never carries hosted tools regardless of the flag: {lite}"
    );
}

/// LAW (LE3, openai half): the session effort MERGES into the ONE reasoning
/// object — on lite the live-confirmed contract survives intact
/// (`summary: auto` kept, `context: all_turns` kept, `max_output_tokens`
/// still absent, `parallel_tool_calls` still false), on the API-key path
/// `summary` survives beside the merged effort, and a NON-reasoning model
/// never grows an effort key.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): replace the merge
/// with `object.insert("reasoning", {"effort": ...})`. Expected runtime
/// failure: the summary/context preservation assertions below.
#[test]
fn effort_merges_into_reasoning_preserving_the_lite_contract() {
    let payload = responses_request_json(&probe_request("gpt-5.6-sol"), true, Some("xhigh"), false)
        .expect("lite payload with effort");
    let object = payload.as_object().expect("object");
    let reasoning = payload
        .get("reasoning")
        .and_then(serde_json::Value::as_object)
        .expect("reasoning object");
    assert_eq!(
        reasoning.get("effort").and_then(serde_json::Value::as_str),
        Some("xhigh"),
        "the session effort merges into reasoning: {payload}"
    );
    assert_eq!(
        reasoning.get("summary").and_then(serde_json::Value::as_str),
        Some("auto"),
        "summary must survive the effort merge: {payload}"
    );
    assert_eq!(
        reasoning.get("context").and_then(serde_json::Value::as_str),
        Some("all_turns"),
        "the lite-required context must survive the effort merge: {payload}"
    );
    assert!(
        !object.contains_key("max_output_tokens"),
        "lite still REJECTS max_output_tokens: {payload}"
    );
    assert_eq!(
        object.get("parallel_tool_calls"),
        Some(&serde_json::Value::Bool(false)),
        "lite still requires parallel_tool_calls=false"
    );
    assert_eq!(
        payload.get("include"),
        Some(&serde_json::json!(["reasoning.encrypted_content"])),
        "the encrypted-content include is unchanged"
    );

    let api_key =
        responses_request_json(&probe_request("gpt-5.6-sol"), false, Some("medium"), false)
            .expect("api-key payload with effort");
    assert_eq!(api_key["reasoning"]["effort"], "medium");
    assert_eq!(api_key["reasoning"]["summary"], "auto");
    assert_eq!(
        api_key["reasoning"].get("context"),
        None,
        "no lite context leaks onto the API-key path"
    );

    // A non-reasoning model on lite keeps the context-only object: effort is
    // gated on the reasoning heuristic, never invented for a model that
    // cannot take it.
    let plain = responses_request_json(&probe_request("gpt-4.1-mini"), true, Some("high"), false)
        .expect("non-reasoning lite payload");
    assert_eq!(
        plain["reasoning"],
        serde_json::json!({"context": "all_turns"}),
        "non-reasoning lite keeps the bare context object: {plain}"
    );
}

/// The API-key Responses path is UNCHANGED: it still sends the output cap and
/// never the lite-only fields.
#[test]
fn api_key_payload_keeps_max_output_tokens_and_no_lite_fields() {
    let payload = responses_request_json(&probe_request("gpt-5.6-sol"), false, None, false)
        .expect("api-key payload");
    let object = payload.as_object().expect("object");
    assert_eq!(
        object
            .get("max_output_tokens")
            .and_then(|value| value.as_u64()),
        Some(200_000),
        "the API-key path keeps its output cap"
    );
    assert!(
        !object.contains_key("parallel_tool_calls"),
        "the lite-only field must not leak onto the API-key path"
    );
    assert_eq!(
        object
            .get("instructions")
            .and_then(serde_json::Value::as_str),
        Some("Be terse."),
        "the API-key path keeps top-level instructions"
    );
    assert_eq!(
        payload["input"][0]["role"], "user",
        "the API-key path must not gain a leading developer item"
    );
    assert_eq!(
        payload
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("context")),
        None,
        "no all_turns context on the API-key path"
    );
}

/// MUTATION CHECK: route Kimi through the generic dialect, omit Bearer, or
/// enable thinking by default. Expected RUNTIME failure: the request golden,
/// header sensitivity, or opt-in extension assertion changes.
#[test]
fn kimi_requests_use_bearer_and_max_completion_tokens() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("kimi-payload-fixture");
    vault
        .put(&alias, b"KIMI_ACCESS_SENTINEL_2d47")
        .expect("store Kimi bearer");
    let credential = vault.resolve(&alias).expect("resolve Kimi bearer");
    let provider = OpenAiCompatibleProvider::new_kimi_subscription(
        credential,
        "kimi-coding-a",
        KIMI_OAUTH_BASE_URL,
    )
    .expect("construct Kimi adapter");
    let request = TurnRequest {
        messages: vec![crate::Message::user_text("hello")],
        model: "kimi-coding-a".to_owned(),
        max_tokens: 1,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: Some(cm2_cache_metadata(KIMI_OAUTH_PROVIDER_NAME, 1)),
    };
    let mut expected: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/openai/kimi_request.json"))
            .expect("Kimi request fixture");
    let payload = provider.request_payload(&request).expect("Kimi payload");
    // Routing metadata is content-addressed below; keep the sanctioned model-
    // visible fixture byte-identical and compare its non-routing shape.
    expected["prompt_cache_key"] = payload["prompt_cache_key"].clone();
    assert_eq!(payload, expected);
    assert!(payload.get("max_tokens").is_none());
    assert!(payload.get("thinking").is_none());
    let authorization = provider
        .http
        .authorization_header()
        .expect("Kimi authorization header");
    assert_eq!(
        authorization.as_bytes(),
        b"Bearer KIMI_ACCESS_SENTINEL_2d47"
    );
    assert!(authorization.is_sensitive());

    let first_key = payload["prompt_cache_key"]
        .as_str()
        .expect("Kimi session cache key");
    let mut next_turn = request.clone();
    next_turn
        .messages
        .push(Message::assistant(vec![Block::Text {
            text: "append-only history".into(),
        }]));
    let next_key = provider
        .request_payload(&next_turn)
        .expect("next Kimi turn")["prompt_cache_key"]
        .as_str()
        .expect("next Kimi cache key")
        .to_owned();
    assert_eq!(
        first_key, next_key,
        "append-only turns share the cohort key"
    );
    next_turn
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .session_scope = "session-b".into();
    let other_session_key = provider
        .request_payload(&next_turn)
        .expect("other Kimi session")["prompt_cache_key"]
        .as_str()
        .expect("other Kimi cache key")
        .to_owned();
    assert_ne!(
        first_key, other_session_key,
        "unrelated same-account Kimi sessions must not share a cohort key"
    );
    next_turn
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .cache_cohort = Some("session-a".into());
    let inherited_session_key = provider
        .request_payload(&next_turn)
        .expect("inherited Kimi session")["prompt_cache_key"]
        .as_str()
        .expect("inherited Kimi cache key")
        .to_owned();
    assert_eq!(
        first_key, inherited_session_key,
        "an inherited Kimi fork shares the parent root cohort"
    );
    next_turn
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .account_scope = Some("account-b".into());
    let other_account_key = provider
        .request_payload(&next_turn)
        .expect("other Kimi account")["prompt_cache_key"]
        .as_str()
        .expect("other Kimi account key")
        .to_owned();
    assert_ne!(
        first_key, other_account_key,
        "Kimi cohorts must not cross accounts"
    );

    let thinking = provider
        .with_kimi_thinking(KimiThinkingConfig {
            thinking_type: KimiThinkingType::Enabled,
            effort: Some("high".to_owned()),
            keep: Some("all".to_owned()),
        })
        .expect("enable Kimi thinking");
    let payload = thinking
        .request_payload(&request)
        .expect("Kimi thinking payload");
    assert_eq!(payload["thinking"]["type"], "enabled");
    assert_eq!(payload["thinking"]["effort"], "high");
    assert_eq!(payload["thinking"]["keep"], "all");
}

/// WH2 — a named DeepSeek turn is rooted at `/chat/completions`, carries a
/// sensitive Bearer key, and sends the selected DeepSeek slug in `model`.
///
/// CM2f also rides rich cache metadata through this unsupported endpoint and
/// proves the existing golden remains byte-exact with no annotation.
/// MUTATION CHECK: route through generic compatible endpoint expansion,
/// remove Bearer, substitute the configured model, or annotate the unknown
/// dialect. The exact URL/header and request golden all fail independently.
#[tokio::test]
async fn wh2_deepseek_request_golden_uses_chat_completions_bearer_and_model() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("deepseek-request-golden");
    vault
        .put(&alias, b"DEEPSEEK_API_KEY_SENTINEL_3d72")
        .expect("store DeepSeek key");
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [93, 184, 216, 34],
        443,
    ))]]));
    let provider = OpenAiCompatibleProvider::new_deepseek_api_with_dns_resolver(
        vault.resolve(&alias).expect("resolve DeepSeek key"),
        "deepseek-reasoner",
        DEEPSEEK_BASE_URL,
        resolver,
    )
    .expect("construct named DeepSeek adapter");
    let request = TurnRequest {
        messages: vec![crate::Message::user_text("hello")],
        model: "deepseek-reasoner".to_owned(),
        max_tokens: 17,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: Some(cm2_cache_metadata(DEEPSEEK_PROVIDER_NAME, 1)),
    };
    let payload = provider
        .request_payload(&request)
        .expect("DeepSeek request payload");
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/openai/deepseek_request.json"
    ))
    .expect("DeepSeek request golden");
    assert_eq!(payload, expected);
    assert_eq!(payload["model"], "deepseek-reasoner");
    assert!(payload.get("temperature").is_none());

    let outbound = provider
        .inference_request(&request)
        .await
        .expect("build fixed DeepSeek inference request");
    assert_eq!(
        outbound.url().as_str(),
        "https://api.deepseek.com/chat/completions"
    );
    let authorization = outbound
        .headers()
        .get(AUTHORIZATION)
        .expect("DeepSeek bearer header");
    assert_eq!(
        authorization.as_bytes(),
        b"Bearer DEEPSEEK_API_KEY_SENTINEL_3d72"
    );
    assert!(authorization.is_sensitive());
    assert!(
        !outbound.headers().contains_key(XAI_CONVERSATION_ID_HEADER),
        "healthy non-xAI Chat Completions requests remain unchanged"
    );
}

/// MUTATION CHECK: route Haider Code through a configurable origin, omit the
/// bearer header, or rewrite its selected model. Expected runtime failure:
/// the exact fixed URL, sensitive authorization bytes, or payload model
/// assertion changes.
#[tokio::test]
async fn haider_code_request_uses_fixed_chat_completions_bearer_and_model() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("haider-code-request-golden");
    vault
        .put(&alias, b"HAIDER_CODE_API_KEY_SENTINEL_3d72")
        .expect("store Haider Code key");
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [93, 184, 216, 34],
        443,
    ))]]));
    let provider = OpenAiCompatibleProvider::new_haider_code_api_with_dns_resolver(
        vault.resolve(&alias).expect("resolve Haider Code key"),
        "Go Max",
        HAIDER_CODE_BASE_URL,
        resolver,
    )
    .expect("construct Haider Code adapter");
    let request = TurnRequest {
        messages: vec![crate::Message::user_text("hello")],
        model: "Go Max".to_owned(),
        max_tokens: 17,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    };
    let payload = provider
        .request_payload(&request)
        .expect("Haider Code request payload");
    assert_eq!(payload["model"], "Go Max");
    assert_eq!(payload["stream"], true);

    let outbound = provider
        .http
        .post_json_body_request(&provider.chat_url, serialized_json_body(payload))
        .await
        .expect("build fixed Haider Code request");
    assert_eq!(
        outbound.url().as_str(),
        "https://haidercode.ai/v1/chat/completions"
    );
    let authorization = outbound
        .headers()
        .get(AUTHORIZATION)
        .expect("Haider Code bearer header");
    assert_eq!(
        authorization.as_bytes(),
        b"Bearer HAIDER_CODE_API_KEY_SENTINEL_3d72"
    );
    assert!(authorization.is_sensitive());
}

/// MUTATION CHECK: deleting any proxy identity header, applying the model
/// override to discovery, or changing the single admitted client version
/// breaks these exact request assertions.
#[tokio::test]
async fn grok_oauth_proxy_request_pins_complete_header_contract() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("grok-oauth-header-contract");
    vault
        .put(&alias, b"GROK_OAUTH_BEARER_SENTINEL_294e")
        .expect("store Grok bearer");
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [93, 184, 216, 34],
        443,
    ))]]));
    let provider = OpenAiCompatibleProvider::new_grok_subscription_with_dns_resolver(
        vault.resolve(&alias).expect("resolve Grok bearer"),
        "grok-4.6",
        GROK_OAUTH_BASE_URL,
        resolver,
    )
    .expect("construct Grok subscription adapter");
    let request = TurnRequest {
        messages: vec![crate::Message::user_text("hello")],
        model: "grok-4.6".to_owned(),
        max_tokens: 17,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    };
    let payload = provider.request_payload(&request).expect("Grok payload");
    let outbound = provider
        .http
        .post_json_body_request(&provider.chat_url, serialized_json_body(payload))
        .await
        .expect("build Grok proxy request");
    assert_eq!(
        outbound.url().as_str(),
        "https://cli-chat-proxy.grok.com/v1/chat/completions"
    );
    let headers = outbound.headers();
    assert_eq!(
        headers.get(AUTHORIZATION).expect("bearer").as_bytes(),
        b"Bearer GROK_OAUTH_BEARER_SENTINEL_294e"
    );
    assert_eq!(headers["x-grok-client-identifier"], "grok-shell");
    assert_eq!(headers["x-grok-client-version"], "0.2.101");
    assert_eq!(headers["x-grok-client-mode"], "interactive");
    assert_eq!(headers["X-XAI-Token-Auth"], "xai-grok-cli");
    assert_eq!(headers["x-grok-model-override"], "grok-4.6");
    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    assert_eq!(
        headers[reqwest::header::USER_AGENT],
        format!("grok-shell/0.2.101 ({platform})")
    );

    let discovery = provider
        .http
        .get_request(&provider.models_url)
        .await
        .expect("build Grok catalog request");
    assert!(discovery.headers().contains_key("x-grok-client-version"));
    assert!(!discovery.headers().contains_key("x-grok-model-override"));
}

/// HAIDER952XAI(a) + CACHE ITEM 962. MUTATION CHECK: remove the xAI inference
/// header, derive it from a turn-varying component, restore unrelated-session
/// sharing, cross an account boundary, or apply it to model discovery.
#[tokio::test]
async fn xai_inference_uses_stable_opaque_cohort_cache_route() {
    use haider_protocol::provider::CacheControlObservationV1;

    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("xai-cache-route");
    vault
        .put(&alias, b"XAI_API_KEY_SENTINEL_952")
        .expect("store xAI key");
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [93, 184, 216, 34],
        443,
    ))]]));
    let provider = OpenAiCompatibleProvider::new_xai_api_with_dns_resolver(
        vault.resolve(&alias).expect("resolve xAI key"),
        "grok-4.6",
        XAI_BASE_URL,
        resolver,
    )
    .expect("construct xAI adapter");
    let mut request = probe_request("grok-4.6");
    request.cache_metadata = Some(cm2_cache_metadata(XAI_PROVIDER_NAME, 1));

    let payload = provider.request_payload(&request).expect("xAI payload");
    assert!(
        payload.get("prompt_cache_key").is_none(),
        "Chat Completions uses a routing header, not the Responses body key"
    );
    assert_eq!(payload["messages"][0]["role"], "system");
    let first = provider
        .inference_request(&request)
        .await
        .expect("first xAI inference request");
    let first_id = first
        .headers()
        .get(XAI_CONVERSATION_ID_HEADER)
        .expect("xAI conversation routing header")
        .to_str()
        .expect("ASCII conversation ID")
        .to_owned();
    assert_eq!(first_id.len(), 64, "conversation ID is BLAKE3 hex");
    assert!(first_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(first_id, "session-a", "raw session scope stays private");
    let prepared = crate::Provider::prepare_turn(&provider, &request).expect("prepared xAI turn");
    assert_eq!(
        *prepared.cache_control(),
        CacheControlObservationV1::Emitted { ttl_ms: None }
    );

    request
        .messages
        .push(crate::Message::assistant(vec![Block::Text {
            text: "PINGACK".into(),
        }]));
    request.messages.push(crate::Message::user_text("again"));
    let metadata = request.cache_metadata.as_mut().expect("cache metadata");
    metadata.compaction_epoch = "new-compaction-epoch".into();
    metadata.prefix_digests.immutable_history = "grown-history".into();
    let next = provider
        .inference_request(&request)
        .await
        .expect("next xAI inference request");
    assert_eq!(
        next.headers()[XAI_CONVERSATION_ID_HEADER],
        first_id,
        "one cohort keeps its route across append-only turns and cache epochs"
    );

    request
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .session_scope = "session-b".into();
    let other = provider
        .inference_request(&request)
        .await
        .expect("other xAI session request");
    assert_ne!(
        other.headers()[XAI_CONVERSATION_ID_HEADER],
        first_id,
        "unrelated same-account xAI sessions must not share routing"
    );
    request
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .cache_cohort = Some("session-a".into());
    let inherited = provider
        .inference_request(&request)
        .await
        .expect("inherited xAI fork request");
    assert_eq!(
        inherited.headers()[XAI_CONVERSATION_ID_HEADER],
        first_id,
        "an inherited xAI fork shares the parent root route"
    );

    request
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .account_scope = Some("account-b".into());
    let other_account = provider
        .inference_request(&request)
        .await
        .expect("other xAI account request");
    assert_ne!(
        other_account.headers()[XAI_CONVERSATION_ID_HEADER],
        first_id,
        "xAI cohorts must not cross accounts"
    );

    let discovery = provider
        .http
        .get_request(&provider.models_url)
        .await
        .expect("xAI discovery request");
    assert!(
        !discovery.headers().contains_key(XAI_CONVERSATION_ID_HEADER),
        "conversation routing applies only to inference"
    );
}

/// MUTATION CHECK: treating the hard version gate as billing/transport or
/// omitting the actionable pinned version breaks both status cases.
#[test]
fn grok_oauth_402_and_426_are_actionable_version_gate_errors() {
    for status in [402, 426] {
        let ordinary = replay_openai_http_error(status, None, br#"{"error":{}}"#);
        let error = grok_version_gate_error(status, ordinary);
        assert_eq!(error.kind, ProviderErrorKind::ConnectionConfiguration);
        assert!(!error.retryable);
        assert!(error.message.contains(GROK_SHELL_CLIENT_VERSION));
        assert!(error.message.contains(&format!("HTTP {status}")));
        assert_eq!(error.presentation.provider_http_status, Some(status));
    }
}

/// xAI uses the standard nested OpenAI cached-token telemetry, so no
/// provider-specific usage fork may discard it.
#[test]
fn xai_cached_prompt_details_map_to_normalized_usage() {
    let usage = chat_usage(
        &serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 7,
            "prompt_tokens_details": {"cached_tokens": 40}
        }),
        None,
        CompatibleDialect::XaiApi,
    )
    .expect("xAI usage");
    assert_eq!(usage.cached, 40);
    let normalized = usage.normalized.expect("normalized xAI usage");
    assert_eq!(normalized.logical_input, 100);
    assert_eq!(normalized.uncached_input, 60);
    assert_eq!(normalized.cache_read_input, 40);
}

/// A user-added compatible server runs the generic dialect. Its standard
/// nested OpenAI cache counter must reach normalized usage unchanged.
///
/// MUTATION CHECK: gate nested `cached_tokens` on a built-in dialect. The
/// exact 60/40 split and Present status then fail for custom providers.
#[test]
fn generic_compatible_reads_nested_openai_cache_usage() {
    let usage = chat_usage(
        &serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 7,
            "prompt_tokens_details": {"cached_tokens": 40}
        }),
        None,
        CompatibleDialect::Generic,
    )
    .expect("generic compatible usage");
    let normalized = usage.normalized.expect("normalized generic usage");
    assert_eq!(normalized.uncached_input, 60);
    assert_eq!(normalized.cache_read_input, 40);
    assert_eq!(normalized.cache_status, CacheStatAvailability::Present);
}

/// DeepSeek-shaped counters are selected by the fields that arrived, even
/// through an arbitrary compatible router.
///
/// MUTATION CHECK: restrict hit/miss parsing to the named DeepSeek adapter;
/// the generic route changes from 23/71 to 94/0.
#[test]
fn generic_compatible_reads_deepseek_hit_and_miss_usage() {
    let usage = chat_usage(
        &serde_json::json!({
            "prompt_tokens": 94,
            "completion_tokens": 13,
            "prompt_cache_miss_tokens": 23,
            "prompt_cache_hit_tokens": 71
        }),
        None,
        CompatibleDialect::Generic,
    )
    .expect("generic DeepSeek-shaped usage");
    let normalized = usage.normalized.expect("normalized generic usage");
    assert_eq!((usage.input, usage.cached), (23, 71));
    assert_eq!(normalized.cache_status, CacheStatAvailability::Present);
}

/// An OpenAI-compatible response without a recognized cache field reports
/// unavailable telemetry; zero remains reserved for an observed zero.
///
/// MUTATION CHECK: default absent `cached_tokens` to zero. The status changes
/// from Unavailable to Present and fabricates a cache hit rate.
#[test]
fn generic_compatible_keeps_absent_cache_telemetry_absent() {
    let usage = chat_usage(
        &serde_json::json!({
            "prompt_tokens": 94,
            "completion_tokens": 13
        }),
        None,
        CompatibleDialect::Generic,
    )
    .expect("generic usage without cache telemetry");
    let normalized = usage.normalized.expect("normalized generic usage");
    assert_eq!(normalized.cache_status, CacheStatAvailability::Unavailable);
    assert_eq!(normalized.cache_telemetry_input, 0);
    assert_eq!(normalized.cache_read_input, 0);
}

/// MUTATION CHECK: skip the normal OpenAI cached-token normalization for the
/// Haider Code dialect. Expected runtime failure: uncached input remains 100
/// or cache-read input becomes zero instead of the exact 60/40 split, and the
/// status is no longer Present.
#[test]
fn haider_code_openai_usage_is_normalized() {
    let usage = chat_usage(
        &serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 7,
            "prompt_tokens_details": {"cached_tokens": 40}
        }),
        None,
        CompatibleDialect::HaiderCodeApi,
    )
    .expect("Haider Code usage");
    let normalized = usage.normalized.expect("normalized Haider Code usage");
    assert_eq!(normalized.logical_input, 100);
    assert_eq!(normalized.uncached_input, 60);
    assert_eq!(normalized.cache_read_input, 40);
    assert_eq!(
        normalized.cache_status,
        haider_protocol::provider::CacheStatAvailability::Present
    );
    assert_eq!(usage.cached, 40);
    assert_eq!(usage.output, 7);
}

fn recorded_deepseek_chat_usage() -> serde_json::Value {
    include_str!("../tests/fixtures/openai/deepseek_reasoning_usage.sse")
        .lines()
        .find_map(|line| {
            let payload = line.strip_prefix("data: ")?;
            let frame: serde_json::Value = serde_json::from_str(payload).ok()?;
            frame.get("usage").cloned()
        })
        .expect("recorded DeepSeek usage frame")
}

/// A proxy dialect must decode the DeepSeek wire shape that actually arrived,
/// regardless of the provider label used to configure the local adapter.
///
/// MUTATION CHECK: restrict top-level hit/miss decoding to `DeepSeekApi`.
/// Expected runtime failure: proxy input/cache stay at 94/0 instead of 23/71.
#[test]
fn haider_code_reads_recorded_deepseek_cache_usage() {
    let items = replay_haider_code_chat_sse(include_bytes!(
        "../tests/fixtures/openai/deepseek_reasoning_usage.sse"
    ));
    let usage = items
        .iter()
        .find_map(|item| match item {
            Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
            _ => None,
        })
        .expect("Haider Code usage event");
    let normalized = usage.normalized.as_ref().expect("normalized proxy usage");
    assert_eq!(
        (usage.input, usage.cached),
        (23, 71),
        "proxy must preserve DeepSeek miss/hit accounting"
    );
    assert_eq!(normalized.logical_input, 94);
    assert_eq!(normalized.uncached_input, 23);
    assert_eq!(normalized.cache_read_input, 71);
    assert_eq!(
        normalized.cache_status,
        haider_protocol::provider::CacheStatAvailability::Present
    );
}

/// Missing telemetry is unavailable, which is distinct from an observed zero.
///
/// MUTATION CHECK: default an absent nested `cached_tokens` field to zero.
/// Expected runtime failure: cache status changes from Unavailable to Present.
#[test]
fn haider_code_without_a_recognized_cache_shape_is_unavailable() {
    let usage = chat_usage(
        &serde_json::json!({
            "prompt_tokens": 94,
            "completion_tokens": 13
        }),
        None,
        CompatibleDialect::HaiderCodeApi,
    )
    .expect("Haider Code usage without cache telemetry");
    let normalized = usage.normalized.expect("normalized proxy usage");
    assert_eq!(
        normalized.cache_status,
        haider_protocol::provider::CacheStatAvailability::Unavailable
    );
    assert_eq!(normalized.cache_telemetry_input, 0);
    assert_eq!(normalized.cache_read_input, 0);
    assert_eq!(usage.cached, 0);
}

/// Both DeepSeek counters must be present and sum exactly to prompt_tokens for
/// either a direct session or a proxy carrying the DeepSeek wire shape.
///
/// MUTATION CHECK: accept a hit/miss pair without the reconciliation guard.
/// Expected runtime failure: both dialects report Present with a 24/71 split.
#[test]
fn non_reconciling_deepseek_cache_usage_is_unavailable_for_direct_and_proxy() {
    use haider_protocol::provider::CacheStatAvailability;

    let mut value = recorded_deepseek_chat_usage();
    value["prompt_cache_miss_tokens"] = serde_json::json!(24);
    let actual = [
        CompatibleDialect::DeepSeekApi,
        CompatibleDialect::HaiderCodeApi,
    ]
    .map(|dialect| {
        let usage = chat_usage(&value, None, dialect).expect("malformed DeepSeek usage");
        let normalized = usage.normalized.expect("normalized malformed usage");
        (
            normalized.cache_status,
            normalized.uncached_input,
            normalized.cache_read_input,
        )
    });
    assert_eq!(
        actual,
        [
            (CacheStatAvailability::Unavailable, 94, 0),
            (CacheStatAvailability::Unavailable, 94, 0),
        ]
    );
}

/// A partial DeepSeek pair is evidence of malformed telemetry, not permission
/// to treat the missing half as zero or to fall through to another shape.
///
/// MUTATION CHECK: infer a missing miss counter as prompt_tokens minus hits.
/// Expected runtime failure: both dialects change from Unavailable to Present.
#[test]
fn partial_deepseek_cache_usage_is_unavailable_for_direct_and_proxy() {
    use haider_protocol::provider::CacheStatAvailability;

    let mut value = recorded_deepseek_chat_usage();
    value
        .as_object_mut()
        .expect("usage object")
        .remove("prompt_cache_miss_tokens");
    let statuses = [
        CompatibleDialect::DeepSeekApi,
        CompatibleDialect::HaiderCodeApi,
    ]
    .map(|dialect| {
        chat_usage(&value, None, dialect)
            .expect("partial DeepSeek usage")
            .normalized
            .expect("normalized partial usage")
            .cache_status
    });
    assert_eq!(
        statuses,
        [
            CacheStatAvailability::Unavailable,
            CacheStatAvailability::Unavailable,
        ]
    );
}

/// Direct DeepSeek remains strict: its configured wire contract does not fall
/// back to OpenAI's nested shape when the required hit/miss pair is absent.
///
/// MUTATION CHECK: allow `DeepSeekApi` to fall back to nested cached_tokens.
/// Expected runtime failure: direct DeepSeek reports Present/71 instead of
/// Unavailable/0.
#[test]
fn direct_deepseek_does_not_fall_back_to_openai_cache_details() {
    use haider_protocol::provider::CacheStatAvailability;

    let mut value = recorded_deepseek_chat_usage();
    let object = value.as_object_mut().expect("usage object");
    object.remove("prompt_cache_hit_tokens");
    object.remove("prompt_cache_miss_tokens");
    object.insert(
        "prompt_tokens_details".to_owned(),
        serde_json::json!({"cached_tokens": 71}),
    );
    let usage = chat_usage(&value, None, CompatibleDialect::DeepSeekApi)
        .expect("direct DeepSeek nested-only usage");
    let normalized = usage.normalized.expect("normalized direct usage");
    assert_eq!(normalized.cache_status, CacheStatAvailability::Unavailable);
    assert_eq!(normalized.uncached_input, 94);
    assert_eq!(normalized.cache_read_input, 0);
    assert_eq!(usage.input, 94);
    assert_eq!(usage.cached, 0);
}

/// WH4 — DeepSeek's top-level cache counters are the accounting authority:
/// miss tokens are uncached input and hit tokens are cache reads.
///
/// MUTATION CHECK: swap the two fields or fall back to aggregate
/// `prompt_tokens`; the unequal 23/71/94 assertions kill the mutation.
#[test]
fn wh4_deepseek_cache_usage_maps_hit_and_miss_tokens() {
    let items = replay_deepseek_chat_sse(include_bytes!(
        "../tests/fixtures/openai/deepseek_reasoning_usage.sse"
    ));
    let usage = items
        .iter()
        .find_map(|item| match item {
            Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
            _ => None,
        })
        .expect("DeepSeek usage event");
    assert_eq!(usage.input, 23, "cache miss is uncached input");
    assert_eq!(usage.cached, 71, "cache hit is cache-read input");
    assert_eq!(usage.input + usage.cached, 94, "matches prompt total");
    assert_eq!(usage.output, 13);
    let normalized = usage
        .normalized
        .as_ref()
        .expect("normalized DeepSeek usage");
    assert_eq!(normalized.logical_input, 94);
    assert_eq!(normalized.uncached_input, 23);
    assert_eq!(normalized.cache_read_input, 71);
}

/// CM1b/CM1c/CM1d — captured Responses usage proves subset subtraction,
/// reported-zero versus omitted telemetry, GPT-5.6 write decoding, and the
/// malformed cached>total fence.
///
/// MUTATION CHECK (executed): replace `total - cached` with `total + cached`,
/// treat absent cached tokens as zero, or saturate malformed subtraction;
/// the 30/70, Present/Unavailable, and malformed assertions fail.
#[test]
fn cm1b_cm1c_cm1d_openai_subset_availability_and_malformed_laws() {
    use haider_protocol::provider::{CacheStatAvailability, StreamEvent};

    let decode = |bytes: &[u8]| {
        replay_openai_responses_sse(bytes)
            .into_iter()
            .find_map(|event| match event {
                Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
                _ => None,
            })
            .expect("captured usage")
    };
    let present = decode(include_bytes!(
        "../tests/fixtures/openai/cache_write_usage.sse"
    ));
    let normalized = present.normalized.as_ref().expect("normalized usage");
    assert_eq!(normalized.logical_input, 100);
    assert_eq!(normalized.uncached_input, 30);
    assert_eq!(normalized.cache_read_input, 70);
    assert_eq!(normalized.cache_write_input, 20);
    assert_eq!(normalized.cache_status, CacheStatAvailability::Present);

    let zero = decode(include_bytes!(
        "../tests/fixtures/openai/reasoning_continuation.sse"
    ));
    assert_eq!(
        zero.normalized
            .as_ref()
            .expect("zero normalized")
            .cache_status,
        CacheStatAvailability::Present,
        "reported zero is present"
    );
    let missing = decode(include_bytes!(
        "../tests/fixtures/openai/missing_cache_usage.sse"
    ));
    assert_eq!(
        missing
            .normalized
            .as_ref()
            .expect("missing normalized")
            .cache_status,
        CacheStatAvailability::Unavailable,
        "omitted cache telemetry is n/a"
    );
    let malformed = decode(include_bytes!(
        "../tests/fixtures/openai/malformed_cache_usage.sse"
    ));
    let malformed = malformed.normalized.as_ref().expect("malformed normalized");
    assert_eq!(malformed.cache_status, CacheStatAvailability::Unavailable);
    assert_eq!(malformed.cache_read_input, 0);
    assert_eq!(malformed.uncached_input, 100);
}

/// Kimi's recorded top-level cache counter is a subset of prompt_tokens, and
/// pass-through adapters select it from the shape rather than their label.
///
/// MUTATION CHECK: restrict top-level `cached_tokens` to `KimiOAuth`.
/// Expected runtime failure: generic and Haider Code report Unavailable/0.
#[test]
fn cm1b_kimi_top_level_cached_tokens_are_shape_driven() {
    use haider_protocol::provider::{CacheStatAvailability, StreamEvent};

    let usage = replay_kimi_chat_sse(include_bytes!(
        "../tests/fixtures/openai/kimi_cache_usage.sse"
    ))
    .into_iter()
    .find_map(|event| match event {
        Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
        _ => None,
    })
    .expect("Kimi usage");
    let normalized = usage.normalized.expect("normalized Kimi usage");
    assert_eq!(normalized.uncached_input, 25);
    assert_eq!(normalized.cache_read_input, 75);
    assert_eq!(normalized.cache_status, CacheStatAvailability::Present);

    for usage in [
        replay_openai_chat_sse(include_bytes!(
            "../tests/fixtures/openai/kimi_cache_usage.sse"
        )),
        replay_haider_code_chat_sse(include_bytes!(
            "../tests/fixtures/openai/kimi_cache_usage.sse"
        )),
    ] {
        let usage = usage
            .into_iter()
            .find_map(|event| match event {
                Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
                _ => None,
            })
            .expect("pass-through Kimi usage");
        let normalized = usage.normalized.expect("normalized pass-through usage");
        assert_eq!(normalized.uncached_input, 25);
        assert_eq!(normalized.cache_read_input, 75);
        assert_eq!(normalized.cache_status, CacheStatAvailability::Present);
    }
}

/// Kimi's official client accepts the standard nested OpenAI counter when the
/// native top-level field is absent.
///
/// MUTATION CHECK: retain the old Kimi-only top-level branch. Expected runtime
/// failure: the nested recorded fixture reports Unavailable/0 instead of 5.
#[test]
fn kimi_reads_recorded_openai_nested_cache_usage() {
    use haider_protocol::provider::{CacheStatAvailability, StreamEvent};

    let usage = replay_kimi_chat_sse(include_bytes!("../tests/fixtures/openai/chat.sse"))
        .into_iter()
        .find_map(|event| match event {
            Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
            _ => None,
        })
        .expect("Kimi OpenAI-compatible usage");
    let normalized = usage.normalized.expect("normalized Kimi nested usage");
    assert_eq!(normalized.logical_input, 33);
    assert_eq!(normalized.uncached_input, 28);
    assert_eq!(normalized.cache_read_input, 5);
    assert_eq!(normalized.cache_status, CacheStatAvailability::Present);
}

/// Once Kimi's top-level field is present it is authoritative: malformed
/// telemetry cannot hide behind a valid nested counter.
///
/// MUTATION CHECK: fall through to nested `cached_tokens` when the top-level
/// value is malformed. Expected runtime failure: status becomes Present.
#[test]
fn malformed_kimi_top_level_cache_usage_does_not_fall_back() {
    use haider_protocol::provider::CacheStatAvailability;

    let usage = chat_usage(
        &serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 7,
            "cached_tokens": "75",
            "prompt_tokens_details": {"cached_tokens": 75}
        }),
        None,
        CompatibleDialect::HaiderCodeApi,
    )
    .expect("malformed Kimi usage");
    let normalized = usage.normalized.expect("normalized malformed usage");
    assert_eq!(normalized.cache_status, CacheStatAvailability::Unavailable);
    assert_eq!(normalized.uncached_input, 100);
    assert_eq!(normalized.cache_read_input, 0);
}

/// WH5 — DeepSeek reasoner streams non-namespaced `reasoning_content`; it
/// must surface as normalized reasoning rather than disappearing.
#[test]
fn wh5_deepseek_reasoning_content_surfaces_as_reasoning() {
    let items = replay_deepseek_chat_sse(include_bytes!(
        "../tests/fixtures/openai/deepseek_reasoning_usage.sse"
    ));
    assert!(items.iter().any(|item| matches!(
        item,
        Ok(StreamEvent::ReasoningDelta { text }) if text == "check the invariant"
    )));
}

/// LAW (LE3, kimi half): a k3-style pair takes the documented TOP-LEVEL
/// `reasoning_effort` knob — no `thinking` object rides along unless the
/// catalog-gated factory also enabled it — and the seam is Kimi-only: the
/// generic compatible dialect rejects it exactly like `with_kimi_thinking`.
#[test]
fn kimi_reasoning_effort_is_top_level_and_kimi_only() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("kimi-reasoning-effort-fixture");
    vault
        .put(&alias, b"KIMI_ACCESS_SENTINEL_8e11")
        .expect("store Kimi bearer");
    let credential = vault.resolve(&alias).expect("resolve Kimi bearer");
    let provider =
        OpenAiCompatibleProvider::new_kimi_subscription(credential, "kimi-k3", KIMI_OAUTH_BASE_URL)
            .expect("construct Kimi adapter")
            .with_kimi_reasoning_effort("max")
            .expect("enable reasoning_effort");
    let request = TurnRequest {
        messages: vec![crate::Message::user_text("hello")],
        model: "kimi-k3".to_owned(),
        max_tokens: 1,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    };
    let payload = provider.request_payload(&request).expect("Kimi payload");
    assert_eq!(payload["reasoning_effort"], "max");
    assert!(
        payload.get("thinking").is_none(),
        "reasoning_effort rides alone: {payload}"
    );

    let generic_vault = MemoryVault::new();
    let generic_alias = CredentialAlias::new("generic-reasoning-effort-fixture");
    generic_vault
        .put(&generic_alias, b"GENERIC_KEY_SENTINEL_71aa")
        .expect("store generic key");
    let generic = OpenAiCompatibleProvider::new(
        generic_vault
            .resolve(&generic_alias)
            .expect("resolve generic key"),
        "generic-model",
        "https://api.example.invalid/v1",
    )
    .expect("construct generic adapter");
    let refused = generic
        .with_kimi_reasoning_effort("max")
        .expect_err("generic dialect must reject the Kimi seam");
    assert_eq!(refused.kind, ProviderErrorKind::InvalidRequest);
}

/// MUTATION CHECK: infer Kimi features from the model slug or expose stronger
/// tool semantics than the provider declares. Expected RUNTIME failure: the
/// fixture-derived capability document changes below.
#[test]
fn kimi_capabilities_are_derived_only_from_catalog_flags() {
    let capabilities = replay_kimi_models_response(
        "kimi-coding-a",
        include_bytes!("../tests/fixtures/catalog/kimi_models.json"),
    )
    .expect("Kimi capabilities");
    assert_eq!(capabilities.provider, KIMI_OAUTH_PROVIDER_NAME);
    assert_eq!(capabilities.context_limit, 262_144);
    assert_eq!(capabilities.vision, FeatureResolve::Native);
    assert_eq!(capabilities.thinking_visible, FeatureResolve::Native);
    assert_eq!(capabilities.parallel_tools, FeatureResolve::Unsupported);
    assert_eq!(
        capabilities.streaming_tool_args,
        FeatureResolve::Unsupported
    );
}

/// MUTATION CHECK: pin Kimi's fixed-origin guard only to chat, widen it to
/// the whole origin, or omit Bearer on model discovery. Expected RUNTIME
/// failure: one required request is rejected, the unrelated path is accepted,
/// or the credential-bearing headers/URLs differ.
#[tokio::test]
async fn kimi_fixed_origin_allows_exact_chat_and_models_endpoints() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("kimi-fixed-origin-fixture");
    vault
        .put(&alias, b"KIMI_FIXED_ACCESS_SENTINEL_981a")
        .expect("store Kimi fixed bearer");
    let resolver = Arc::new(StubDnsResolver::new([vec![SocketAddr::from((
        [93, 184, 216, 34],
        443,
    ))]]));
    let provider = OpenAiCompatibleProvider::new_kimi_subscription_with_dns_resolver(
        vault.resolve(&alias).expect("resolve Kimi fixed bearer"),
        "kimi-coding-a",
        KIMI_OAUTH_BASE_URL,
        resolver.clone(),
    )
    .expect("construct fixed Kimi adapter");

    let chat = provider
        .http
        .post_json_body_request(
            &provider.chat_url,
            serialized_json_body(serde_json::json!({"bounded": true})),
        )
        .await
        .expect("exact chat endpoint");
    assert_eq!(
        chat.url().as_str(),
        "https://api.kimi.com/coding/v1/chat/completions"
    );
    let models = provider
        .http
        .get_request(&provider.models_url)
        .await
        .expect("exact models endpoint");
    assert_eq!(
        models.url().as_str(),
        "https://api.kimi.com/coding/v1/models"
    );
    let authorization = models
        .headers()
        .get(AUTHORIZATION)
        .expect("models bearer header");
    assert_eq!(
        authorization.as_bytes(),
        b"Bearer KIMI_FIXED_ACCESS_SENTINEL_981a"
    );
    assert!(authorization.is_sensitive());
    let rejected = provider
        .http
        .get_request("https://api.kimi.com/coding/v1/usages")
        .await
        .expect_err("an unlisted same-origin path stays forbidden");
    assert_eq!(rejected.kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(
        resolver.calls(),
        1,
        "both exact endpoints share one DNS pin"
    );

    assert_eq!(
        unavailable_compatible_capabilities(KIMI_OAUTH_PROVIDER_NAME).provider,
        KIMI_OAUTH_PROVIDER_NAME
    );
}

/// MUTATION CHECK (W5g-6): encode assistant history as `input_text` (the
/// pre-fix shape). Expected runtime failure: the assistant-content
/// assertion below — and against the LIVE endpoint it is a hard 400
/// ("Invalid value: 'input_text'. Supported values are: 'output_text' and
/// 'refusal'", confirmed 2026-07-31), which ERRORED every turn after a
/// session's first assistant reply.
#[test]
fn assistant_history_replays_as_output_text() {
    let request = TurnRequest {
        messages: vec![
            crate::Message::user_text("hi"),
            crate::Message::assistant(vec![crate::Block::Text {
                text: "Hi! How can I help?".to_owned(),
            }]),
            crate::Message::user_text("say PONG"),
        ],
        model: "gpt-5.6-sol".to_owned(),
        max_tokens: 30_000,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    };
    for lite in [true, false] {
        let payload = responses_request_json(&request, lite, None, false).expect("payload");
        let input = payload["input"].as_array().expect("input array");
        let content_type = |index: usize| {
            input[index]["content"][0]["type"]
                .as_str()
                .expect("content type")
                .to_owned()
        };
        assert_eq!(content_type(0), "input_text", "user stays input_text");
        assert_eq!(
            content_type(1),
            "output_text",
            "assistant history is OUTPUT text (lite={lite}): {payload}"
        );
        assert_eq!(content_type(2), "input_text");
    }
}

// ───────────────────────── G4b Azure OpenAI (v1 surface) ────────────────────

/// LAW (LZ1 — the azure header mode): the Azure adapter authenticates with
/// the bare `api-key` header and NO Authorization header on BOTH the models
/// probe and the chat POST, and the DEPLOYMENT NAME rides `body.model` on
/// the derived `{endpoint}/openai/v1/chat/completions` URL.
///
/// MUTATION CHECK: route `AzureApiKey` through the Bearer arm (or drop the
/// header-mode switch in `new_azure`). Expected RUNTIME failure: the
/// `api-key` equality and the no-Authorization assertions below.
#[tokio::test]
async fn lz1_azure_request_rides_api_key_header_and_deployment_model() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("azure-openai-audit");
    vault
        .put(&alias, b"AZURE_API_KEY_SENTINEL_5c1e")
        .expect("store azure key");
    let resolver: Arc<dyn FixedDnsResolver> = Arc::new(StubDnsResolver::new([
        vec![SocketAddr::from(([192, 0, 2, 1], 443))],
        vec![SocketAddr::from(([192, 0, 2, 1], 443))],
    ]));
    let provider = OpenAiCompatibleProvider::new_azure_with_dns_resolver(
        vault.resolve(&alias).expect("resolve azure key"),
        "my-gpt-deployment",
        "https://contoso.openai.azure.com/openai/v1",
        resolver,
    )
    .expect("azure adapter");
    assert_eq!(
        provider.models_url(),
        "https://contoso.openai.azure.com/openai/v1/models"
    );

    let get = provider
        .http
        .get_request(&provider.models_url)
        .await
        .expect("models probe request");
    assert_eq!(
        get.headers().get("api-key").expect("api-key on GET"),
        "AZURE_API_KEY_SENTINEL_5c1e"
    );
    assert!(
        !get.headers().contains_key(AUTHORIZATION),
        "azure keys never ride Authorization"
    );

    let payload = provider
        .request_payload(&TurnRequest {
            messages: vec![crate::Message::user_text("ping")],
            model: "my-gpt-deployment".into(),
            max_tokens: 16,
            system_prompt: None,
            tools: Vec::new(),
            attachments: Vec::new(),
            cache_metadata: None,
        })
        .expect("azure chat payload");
    assert_eq!(
        payload.get("model").and_then(serde_json::Value::as_str),
        Some("my-gpt-deployment"),
        "the DEPLOYMENT NAME rides body.model"
    );
    let post = provider
        .http
        .post_json_body_request(&provider.chat_url, serialized_json_body(payload))
        .await
        .expect("azure chat POST");
    assert_eq!(
        post.url().as_str(),
        "https://contoso.openai.azure.com/openai/v1/chat/completions"
    );
    assert_eq!(
        post.headers().get("api-key").expect("api-key on POST"),
        "AZURE_API_KEY_SENTINEL_5c1e"
    );
    assert!(!post.headers().contains_key(AUTHORIZATION));
}

/// LAW (LZ1, origin half): `new_azure` accepts ONLY https Azure OpenAI
/// resource hosts, and the shared predicate agrees — the header mode can
/// never leak a key to an arbitrary origin, and non-azure adapters keep
/// Bearer untouched.
#[test]
fn azure_origin_predicate_and_constructor_agree_both_directions() {
    for accepted in [
        "https://contoso.openai.azure.com/openai/v1",
        "https://acme-ai.services.ai.azure.com/openai/v1/",
        "https://Contoso.openai.azure.com",
        "HTTPS://contoso.openai.azure.com/openai/v1",
    ] {
        assert!(azure_openai_origin(accepted), "azure origin `{accepted}`");
    }
    for refused in [
        "http://contoso.openai.azure.com/openai/v1",
        "https://openai.azure.com/openai/v1",
        "https://contoso.openai.azure.com.evil.example/openai/v1",
        "https://contoso.evil.example/openai/v1",
        "https://127.0.0.1:8000/v1",
        "",
    ] {
        assert!(!azure_openai_origin(refused), "non-azure `{refused}`");
        let vault = MemoryVault::new();
        let alias = CredentialAlias::new("azure-origin-audit");
        vault
            .put(&alias, b"NEVER_SENT_AZURE_ORIGIN")
            .expect("store sentinel");
        assert!(
            OpenAiCompatibleProvider::new_azure(
                vault.resolve(&alias).expect("resolve sentinel"),
                "deployment",
                refused,
            )
            .is_err(),
            "new_azure must refuse `{refused}`"
        );
    }
}

/// LAW (LW4, alpha/search request-body golden — DELIBERATE re-pin
/// 2026-08-21): the backend drifted to a `commands` OBJECT keyed by command
/// family (the 935-era array form 400s "Invalid type for 'commands':
/// expected an object"). The body pins the codex-0.145-captured shape:
/// session id, model, empty input, one `search_query` entry carrying the
/// query, `response_length: long`, the captured settings, and the captured
/// `max_output_tokens` bound.
#[test]
fn alpha_search_request_body_is_golden() {
    let body = codex_alpha_search_request_body("session-9", "gpt-5.6-sol", "rust sse decoding");
    assert_eq!(
        body,
        serde_json::json!({
            "id": "session-9",
            "model": "gpt-5.6-sol",
            "input": [],
            "commands": {
                "search_query": [{"q": "rust sse decoding"}],
                "response_length": "long",
            },
            "settings": {
                "allowed_callers": ["direct"],
                "external_web_access": false,
            },
            "max_output_tokens": 10000,
        })
    );
    assert_eq!(
        OPENAI_ALPHA_SEARCH_URL,
        "https://chatgpt.com/backend-api/codex/alpha/search"
    );

    // Response extraction stays tolerant: output-item text, top-level
    // text, and raw-JSON fallback all yield bounded readable text.
    let items = serde_json::json!({
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "first"}]},
            {"type": "note", "text": "second"},
        ],
    });
    let text = codex_alpha_search_response_text(items.to_string().as_bytes());
    assert!(text.contains("first") && text.contains("second"), "{text}");
    assert_eq!(
        codex_alpha_search_response_text(br#"{"output_text":"top level"}"#),
        "top level"
    );
    assert_eq!(
        codex_alpha_search_response_text(b"not json at all"),
        "not json at all"
    );
}

/// LAW (W-F M5, public-IP classifier sweep — BOTH directions): the special-use
/// ranges added in W-F are BLOCKED, and a representative public address just
/// outside each stays ALLOWED. The web_fetch public fence and the fixed-origin
/// fence both derive from this classifier, so extending it here closes them
/// all at once. TEST-NET (192.0.2/24, 198.51.100/24, 203.0.113/24) stays
/// classified PUBLIC on purpose — those are guaranteed-never-routed
/// documentation ranges used as safe public stand-ins by the origin laws.
#[test]
fn m5_classifier_blocks_added_special_use_ranges_both_directions() {
    let v4 = |a, b, c, d| std::net::IpAddr::V4(Ipv4Addr::new(a, b, c, d));
    // BLOCKED: in-range representatives (low + high edges where they matter).
    for blocked in [
        v4(100, 64, 0, 1),      // CGNAT / RFC 6598 low edge (100.64/10)
        v4(100, 127, 255, 254), // CGNAT high edge
        v4(198, 18, 0, 1),      // RFC 2544 benchmarking low (198.18/15)
        v4(198, 19, 255, 254),  // RFC 2544 benchmarking high
        v4(192, 0, 0, 1),       // IETF protocol assignments (192.0.0/24)
        v4(240, 0, 0, 1),       // Class E / reserved low (240/4)
        v4(255, 255, 255, 254), // Class E high, below broadcast
    ] {
        assert!(
            blocked_credential_target(blocked),
            "{blocked} is special-use and must be blocked"
        );
    }
    // ALLOWED: nearest public neighbours just outside each added range, plus an
    // ordinary routable literal — the sweep is EXACT, not over-broad.
    for public in [
        v4(100, 63, 255, 254), // just below CGNAT
        v4(100, 128, 0, 1),    // just above CGNAT
        v4(198, 17, 255, 254), // just below 198.18/15
        v4(198, 20, 0, 1),     // just above 198.18/15
        v4(192, 0, 1, 1),      // just above 192.0.0/24
        v4(93, 184, 216, 34),  // ordinary public (example.com)
    ] {
        assert!(
            !blocked_credential_target(public),
            "{public} is public and must stay allowed"
        );
    }
    // NAT64 well-known prefix 64:ff9b::/96 embedding 127.0.0.1 is blocked; an
    // ordinary global-unicast v6 is not.
    let nat64 = "64:ff9b::7f00:1"
        .parse::<std::net::IpAddr>()
        .expect("nat64 literal");
    assert!(
        blocked_credential_target(nat64),
        "NAT64-embedded loopback must be blocked"
    );
    let public_v6 = "2606:2800:220:1:248:1893:25c8:1946"
        .parse::<std::net::IpAddr>()
        .expect("public v6 literal");
    assert!(
        !blocked_credential_target(public_v6),
        "ordinary public v6 must stay allowed"
    );
}
/// LAW E1e: invalid endpoint/configuration failures bypass connection retry.
/// MUTATION: route every reqwest error through Transport and this fails.
#[tokio::test]
async fn e1e_invalid_endpoint_is_permanent_connection_configuration() {
    let reqwest_error = reqwest::Client::new()
        .get("://invalid endpoint")
        .send()
        .await
        .expect_err("invalid URL");
    let error = super::transport_error(reqwest_error);
    assert_eq!(
        error.kind,
        crate::ProviderErrorKind::ConnectionConfiguration
    );
    assert!(!error.retryable);
    assert!(error.message.contains("connection configuration failed"));
}
