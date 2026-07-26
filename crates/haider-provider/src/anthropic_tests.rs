#![allow(clippy::expect_used)]

use std::future;
use std::time::Duration;

use haider_protocol::provider::StreamEvent;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::anthropic::{SseChunkSource, stream_sse_source};
use crate::{ProviderError, ProviderErrorKind};

struct HangingFixture {
    first_chunk: Option<Vec<u8>>,
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
