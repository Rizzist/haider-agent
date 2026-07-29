#![allow(clippy::expect_used)]

use std::future;
use std::process::{Command, Stdio};
use std::time::Duration;

use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::provider::StreamEvent;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::anthropic::{AnthropicProvider, SseChunkSource, stream_sse_source};
use crate::{ProviderError, ProviderErrorKind};

struct HangingFixture {
    first_chunk: Option<Vec<u8>>,
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
