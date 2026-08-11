//! Shared real-UDS support for daemon integration tests.
//!
//! Keep framing, handshake, readiness, and short socket-path setup here so
//! every black-box suite—including W3c's live-turn gate—exercises the same
//! production transport contract.

#![allow(clippy::expect_used)]
// Each test binary compiles this module independently and uses a different
// helper subset, so per-binary dead-code warnings would fire on live
// helpers. The cost: a helper no suite uses anymore will not be flagged —
// re-audit when helpers are added.
#![allow(dead_code)]

use haider_daemon::{
    DaemonConfig, DaemonDependencies, DaemonState, DaemonTask, spawn, spawn_with_dependencies,
};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, Hello, WIRE_PROTOCOL_VERSION, WireFrame, uds_codec,
};
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

// 60s, not 10 (W5f-3): the full per-crate gate runs this suite's daemons
// under heavy compile/test contention, and the 10s ceiling flaked
// `worker_aware_drain_terminalizes_durable_queued_turns_before_store_close`
// three times — always under load, never isolated. A passing run never
// waits; only real failures pay the longer bound.
pub const DEADLINE: Duration = Duration::from_secs(60);

pub fn test_root(prefix: &str) -> tempfile::TempDir {
    #[cfg(target_os = "macos")]
    const SHORT_TMP_ROOT: &str = "/private/tmp";
    #[cfg(not(target_os = "macos"))]
    const SHORT_TMP_ROOT: &str = "/tmp";

    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(SHORT_TMP_ROOT)
        .expect("short temporary root")
}

/// Hermeticity law for every black-box daemon: integration tests must NEVER
/// probe the developer machine's real credential stores (codex auth.json,
/// Claude Keychain, kimi files). Startup auto-adoption (A2) runs whenever
/// discovery is enabled, so the harness forces it off — a suite that ever
/// needs live discovery must spawn directly and inject mock stores.
fn hermetic(config: &DaemonConfig) -> DaemonConfig {
    let mut config = config.clone();
    config.discovery_disabled = true;
    config
}

pub async fn ready(config: &DaemonConfig) -> DaemonTask {
    let task = spawn(hermetic(config));
    await_ready(task).await
}

pub async fn ready_with_dependencies(
    config: &DaemonConfig,
    dependencies: DaemonDependencies,
) -> DaemonTask {
    let task = spawn_with_dependencies(hermetic(config), dependencies);
    await_ready(task).await
}

async fn await_ready(task: DaemonTask) -> DaemonTask {
    let mut readiness = task.readiness();
    let ready = tokio::time::timeout(DEADLINE, async {
        loop {
            if readiness.current() == DaemonState::Ready {
                return Ok(());
            }
            readiness.changed().await.ok_or(())?;
        }
    })
    .await;
    match ready {
        Ok(Ok(())) => task,
        Ok(Err(())) => panic!("daemon stopped before Ready: {:?}", task.join().await),
        Err(_) => panic!("ready deadline"),
    }
}

pub struct UdsClient {
    pub stream: UnixStream,
    decoder: uds_codec::Decoder,
    pending: VecDeque<WireFrame>,
}

impl UdsClient {
    pub async fn connect(path: &Path, frame_limit: usize) -> std::io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path).await?,
            decoder: uds_codec::Decoder::new(frame_limit),
            pending: VecDeque::new(),
        })
    }

    pub async fn connect_control(
        path: &Path,
        frame_limit: usize,
        client_name: &str,
        client_instance_id: &str,
        client_kind: ClientKind,
    ) -> Self {
        Self::connect_with_capabilities(
            path,
            frame_limit,
            client_name,
            client_instance_id,
            client_kind,
            CapabilitySet::from([Capability::View, Capability::Control]),
        )
        .await
    }

    pub async fn connect_with_capabilities(
        path: &Path,
        frame_limit: usize,
        client_name: &str,
        client_instance_id: &str,
        client_kind: ClientKind,
        capabilities_requested: CapabilitySet,
    ) -> Self {
        let mut client = Self::connect(path, frame_limit).await.expect("connect");
        client
            .send(
                &WireFrame::Hello(Hello {
                    protocol_min: WIRE_PROTOCOL_VERSION,
                    protocol_max: WIRE_PROTOCOL_VERSION,
                    client_name: client_name.into(),
                    client_version: "test".into(),
                    client_instance_id: client_instance_id.into(),
                    client_kind,
                    capabilities_requested,
                    max_receive_frame: u32::try_from(frame_limit).expect("frame limit"),
                }),
                frame_limit,
            )
            .await;
        assert!(matches!(client.next().await, WireFrame::Welcome(_)));
        client
    }

    pub async fn send(&mut self, frame: &WireFrame, limit: usize) {
        let bytes = uds_codec::encode(frame, limit).expect("test frame encodes");
        self.stream.write_all(&bytes).await.expect("frame writes");
    }

    /// Best-effort send for retry loops: a rejected connection may already be
    /// closed by the time the test writes.
    pub async fn try_send(&mut self, frame: &WireFrame, limit: usize) -> bool {
        let bytes = uds_codec::encode(frame, limit).expect("test frame encodes");
        self.stream.write_all(&bytes).await.is_ok()
    }

    pub async fn receive(&mut self) -> WireFrame {
        self.try_receive()
            .await
            .expect("connection closed before a frame arrived")
    }

    pub async fn next(&mut self) -> WireFrame {
        tokio::time::timeout(DEADLINE, self.receive())
            .await
            .expect("frame deadline")
    }

    /// Next frame, or `None` when the daemon closed the connection first.
    pub async fn try_receive(&mut self) -> Option<WireFrame> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        loop {
            let mut bytes = [0_u8; 16 * 1024];
            let read = self.stream.read(&mut bytes).await.expect("frame reads");
            if read == 0 {
                return None;
            }
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "server sent an invalid frame");
            self.pending.extend(batch.frames);
            if let Some(frame) = self.pending.pop_front() {
                return Some(frame);
            }
        }
    }

    /// Reads at least `at_least` raw bytes into the decoder without waiting
    /// for a whole frame, leaving a large reply deliberately mid-write.
    pub async fn absorb_at_least(&mut self, at_least: usize) {
        let mut absorbed = 0;
        while absorbed < at_least {
            let mut bytes = [0_u8; 8 * 1024];
            let read = tokio::time::timeout(DEADLINE, self.stream.read(&mut bytes))
                .await
                .expect("partial read deadline")
                .expect("partial read");
            assert_ne!(read, 0, "connection closed before the reply started");
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "server sent an invalid frame");
            self.pending.extend(batch.frames);
            absorbed += read;
        }
    }

    pub async fn expect_eof(&mut self) {
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(DEADLINE, self.stream.read(&mut byte))
            .await
            .expect("EOF deadline")
            .expect("EOF read");
        assert_eq!(read, 0);
    }

    /// Reads until EOF and reports how many complete frames arrived.
    pub async fn frames_until_eof(&mut self) -> usize {
        let mut frames = self.pending.len();
        loop {
            let mut bytes = [0_u8; 16 * 1024];
            let read = tokio::time::timeout(DEADLINE, self.stream.read(&mut bytes))
                .await
                .expect("EOF deadline")
                .expect("EOF read");
            if read == 0 {
                return frames;
            }
            frames += self.decoder.push(&bytes[..read]).frames.len();
        }
    }
}
