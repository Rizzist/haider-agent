#![allow(clippy::expect_used)]

use super::*;
use std::collections::VecDeque;
use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

    let (base_url, chat_url, models_url) =
        endpoint_result.expect("mutation removed metadata origin guard");
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
        },
        base_url,
        chat_url,
        models_url,
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
