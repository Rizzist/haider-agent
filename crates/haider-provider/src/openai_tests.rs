#![allow(clippy::expect_used)]

use super::*;
use std::collections::VecDeque;
use std::future;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use haider_accounts::{MemoryVault, Vault};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::error::TryRecvError;

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
impl CompatibleDnsResolver for StubDnsResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .expect("resolver answer lock")
            .pop_front()
            .ok_or_else(|| std::io::Error::other("stub resolver was called more than expected"))
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
        DecoderKind::Responses,
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
    assert!(error.retryable);
    assert!(error.message.contains("90 seconds"));
    assert!(receiver.recv().await.is_none());
    stream_task.await.expect("stream task exits");
    assert!(dropped.load(Ordering::SeqCst));
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
        DecoderKind::Responses,
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
/// Safe code rejects before `get_request` or `post_json_request` can construct
/// a request carrying the bearer. The mutation makes the metadata-host request
/// observable here with the exact sentinel header.
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
                .post_json_request(
                    &provider.chat_url,
                    &serde_json::json!({"model":"audit-model"}),
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
        .post_json_request(&provider.api_url, &serde_json::json!({"model":"gpt-audit"}))
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
            .post_json_request(&rebound.api_url, &serde_json::json!({"model":"gpt-audit"}))
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
        },
        base_url: endpoints.base_url,
        chat_url: endpoints.chat_url,
        models_url: endpoints.models_url,
        dialect: CompatibleDialect::Generic,
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

/// LAW (LK1 golden half — the placeholder Bearer is SENT): a custom adapter
/// holding the daemon's keyless placeholder secret (`ollama`) emits exactly
/// `Authorization: Bearer ollama` on both the models probe and the chat
/// POST at a loopback literal origin — the header shape ollama's compat
/// layer requires and LM Studio ignores.
///
/// MUTATION CHECK: blank the credential bytes in `authorization_header` (or
/// drop the secret concatenation). Expected RUNTIME failure: the exact
/// header equality below.
#[tokio::test]
async fn lk1_keyless_placeholder_bearer_reaches_the_wire_header() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("ollama-keyless");
    vault.put(&alias, b"ollama").expect("store placeholder");
    let credential = vault.resolve(&alias).expect("resolve placeholder");
    let provider =
        OpenAiCompatibleProvider::new_custom(credential, "llama3.1:8b", "http://127.0.0.1:11434")
            .expect("custom loopback adapter")
            .with_account(alias);

    let get = provider
        .http
        .get_request(&provider.models_url)
        .await
        .expect("models probe request");
    assert_eq!(
        get.headers().get(AUTHORIZATION).expect("bearer on GET"),
        "Bearer ollama"
    );

    let post = provider
        .http
        .post_json_request(
            &provider.chat_url,
            &serde_json::json!({"model":"llama3.1:8b"}),
        )
        .await
        .expect("chat POST request");
    assert_eq!(
        post.headers().get(AUTHORIZATION).expect("bearer on POST"),
        "Bearer ollama"
    );
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
    }
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
    };
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/openai/kimi_request.json"))
            .expect("Kimi request fixture");
    let payload = provider.request_payload(&request).expect("Kimi payload");
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
        .post_json_request(&provider.chat_url, &serde_json::json!({"bounded": true}))
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
    let provider = OpenAiCompatibleProvider::new_azure(
        vault.resolve(&alias).expect("resolve azure key"),
        "my-gpt-deployment",
        "https://contoso.openai.azure.com/openai/v1",
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
        })
        .expect("azure chat payload");
    assert_eq!(
        payload.get("model").and_then(serde_json::Value::as_str),
        Some("my-gpt-deployment"),
        "the DEPLOYMENT NAME rides body.model"
    );
    let post = provider
        .http
        .post_json_request(&provider.chat_url, &payload)
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

/// LAW (LW4, alpha/search request-body golden): the client `web_search`
/// SearchRequest body pins exactly the locked shape — session id, model, an
/// empty input, ONE search command carrying the query, and the locked
/// settings (`medium` context, `direct` caller, external access on) — and
/// never `max_output_tokens` (the same backend bans it on lite).
#[test]
fn alpha_search_request_body_is_golden() {
    let body = codex_alpha_search_request_body("session-9", "gpt-5.6-sol", "rust sse decoding");
    assert_eq!(
        body,
        serde_json::json!({
            "id": "session-9",
            "model": "gpt-5.6-sol",
            "input": [],
            "commands": [{"type": "search", "query": "rust sse decoding"}],
            "settings": {
                "search_context_size": "medium",
                "allowed_callers": ["direct"],
                "external_web_access": true,
            },
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
