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

/// MUTATION CHECK: remove `validate_compatible_origin(&parsed)?`.
///
/// The fallback branch deliberately routes the exact metadata URL through
/// a local proxy and proves the bearer crosses the wire. Safe code returns
/// before any client, socket, or authorization header is constructed.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn metadata_origin_guard_prevents_credential_bearing_request() {
    let endpoint_result = compatible_endpoints("http://169.254.169.254");
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
        },
        base_url: endpoints.base_url,
        chat_url: endpoints.chat_url,
        models_url: endpoints.models_url,
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
    let loopback = compatible_endpoints("http://[::1]:11434")
        .expect("bracketed IPv6 loopback is a valid literal origin");
    assert!(
        loopback.origin.is_none(),
        "[::1] must classify as an IP literal, not a DNS-resolved hostname"
    );

    // A public IPv6 literal over HTTPS is likewise a literal.
    let public = compatible_endpoints("https://[2606:4700:4700::1111]:443")
        .expect("bracketed public IPv6 literal is valid");
    assert!(
        public.origin.is_none(),
        "a public IPv6 literal must classify as a literal"
    );

    // A real hostname (no brackets) still takes the resolve-validate-pin path.
    let domain = compatible_endpoints("https://gateway.example.com:8443")
        .expect("hostname base_url is valid");
    assert!(
        domain.origin.is_some(),
        "a domain must classify as a hostname for resolve-validate-pin"
    );
}
