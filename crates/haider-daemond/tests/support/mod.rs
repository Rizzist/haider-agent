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

#[cfg(windows)]
use haider_daemon::DaemonTaskDiagnostics;
use haider_daemon::{
    DaemonConfig, DaemonDependencies, DaemonState, DaemonTask, spawn, spawn_with_dependencies,
};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, Hello, WIRE_PROTOCOL_VERSION, WireFrame, uds_codec,
};
#[cfg(windows)]
use std::collections::HashMap;
use std::collections::VecDeque;
#[cfg(windows)]
use std::fmt::Write as _;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// 60s, not 10 (W5f-3): the full per-crate gate runs this suite's daemons
// under heavy compile/test contention, and the 10s ceiling flaked
// `worker_aware_drain_terminalizes_durable_queued_turns_before_store_close`
// three times — always under load, never isolated. A passing run never
// waits; only real failures pay the longer bound.
pub const DEADLINE: Duration = Duration::from_secs(60);

/// Active test peers ping well inside the daemon's 45-second read-idle
/// deadline. Long scenario-specific waits may exceed both values; this cadence
/// keeps their connections non-idle for the whole outer wait.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

#[cfg(windows)]
const DIAGNOSTIC_LINES: usize = 200;
#[cfg(windows)]
const DIAGNOSTIC_LINE_BYTES: usize = 4 * 1024;
#[cfg(windows)]
const DIAGNOSTIC_TRACE_PRINT_LINES: usize = 50;

#[cfg(windows)]
type DiagnosticRing = Arc<StdMutex<VecDeque<String>>>;

#[cfg(windows)]
fn daemon_tasks() -> &'static StdMutex<HashMap<PathBuf, DaemonTaskDiagnostics>> {
    static TASKS: OnceLock<StdMutex<HashMap<PathBuf, DaemonTaskDiagnostics>>> = OnceLock::new();
    TASKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

#[cfg(windows)]
fn register_daemon(config: &DaemonConfig, task: &DaemonTask) {
    daemon_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(config.endpoint_path(), task.diagnostics());
}

#[cfg(windows)]
fn daemon_diagnostics(path: &Path) -> Option<DaemonTaskDiagnostics> {
    daemon_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .cloned()
}

#[cfg(windows)]
fn push_diagnostic(ring: &DiagnosticRing, mut line: String) {
    if line.len() > DIAGNOSTIC_LINE_BYTES {
        let mut end = DIAGNOSTIC_LINE_BYTES.saturating_sub('…'.len_utf8());
        while !line.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        line.truncate(end);
        line.push('…');
    }
    let mut output = ring
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if output.len() == DIAGNOSTIC_LINES {
        output.pop_front();
    }
    output.push_back(line);
}

#[cfg(windows)]
fn ring_snapshot(ring: &DiagnosticRing) -> Vec<String> {
    ring.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

#[cfg(windows)]
fn ring_tail(ring: &DiagnosticRing, limit: usize) -> Vec<String> {
    let output = ring
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    output
        .iter()
        .skip(output.len().saturating_sub(limit))
        .cloned()
        .collect()
}

#[cfg(windows)]
fn trace_ring() -> DiagnosticRing {
    static TRACE: OnceLock<DiagnosticRing> = OnceLock::new();
    Arc::clone(TRACE.get_or_init(|| Arc::new(StdMutex::new(VecDeque::new()))))
}

#[cfg(windows)]
struct TraceFields<'a>(&'a mut String);

#[cfg(windows)]
impl tracing::field::Visit for TraceFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?};", field.name());
    }
}

#[cfg(windows)]
struct TraceCapture {
    output: DiagnosticRing,
    next_span: AtomicU64,
}

#[cfg(windows)]
impl tracing::Subscriber for TraceCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut line = format!(
            "span target={};name={};",
            attributes.metadata().target(),
            attributes.metadata().name()
        );
        attributes.record(&mut TraceFields(&mut line));
        push_diagnostic(&self.output, line);
        tracing::span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed).max(1))
    }

    fn record(&self, _span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::from("span-record ");
        values.record(&mut TraceFields(&mut line));
        push_diagnostic(&self.output, line);
    }

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!(
            "event target={};name={};",
            event.metadata().target(),
            event.metadata().name()
        );
        event.record(&mut TraceFields(&mut line));
        push_diagnostic(&self.output, line);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

#[cfg(windows)]
fn install_trace_capture() -> DiagnosticRing {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    let output = trace_ring();
    INSTALLED.get_or_init(|| {
        if tracing::subscriber::set_global_default(TraceCapture {
            output: Arc::clone(&output),
            next_span: AtomicU64::new(1),
        })
        .is_err()
        {
            push_diagnostic(
                &output,
                "daemon trace capture unavailable because a global subscriber already exists"
                    .into(),
            );
        }
        let previous = std::panic::take_hook();
        let panic_output = Arc::clone(&output);
        std::panic::set_hook(Box::new(move |info| {
            push_diagnostic(&panic_output, format!("panic: {info}"));
            previous(info);
        }));
    });
    output
}

/// Prints the bounded in-process daemon trace and wire transcript when a
/// Windows test unwinds. There is no child stdout/stderr or OS ExitStatus in
/// this harness; the trace/panic ring and typed daemon-task outcome are the
/// truthful equivalents.
#[cfg(windows)]
pub struct FailureDiagnostics {
    label: &'static str,
    daemon: DaemonTaskDiagnostics,
    trace: DiagnosticRing,
    clients: Vec<DiagnosticRing>,
}

#[cfg(windows)]
impl FailureDiagnostics {
    pub fn install(label: &'static str, task: &DaemonTask) -> Self {
        Self {
            label,
            daemon: task.diagnostics(),
            trace: install_trace_capture(),
            clients: Vec::new(),
        }
    }

    pub fn watch(&mut self, client: &UdsClient) {
        self.clients.push(Arc::clone(&client.history));
    }
}

#[cfg(windows)]
impl Drop for FailureDiagnostics {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        eprintln!(
            "===== {} Windows daemon failure diagnostics =====",
            self.label
        );
        eprintln!(
            "daemon is in-process (no child stdout/stderr or OS exit status); task={:?}",
            self.daemon.snapshot()
        );
        eprintln!("----- bounded daemon trace/panic output -----");
        for line in ring_tail(&self.trace, DIAGNOSTIC_TRACE_PRINT_LINES) {
            eprintln!("{line}");
        }
        let client_lines = DIAGNOSTIC_LINES.saturating_sub(DIAGNOSTIC_TRACE_PRINT_LINES)
            / self.clients.len().max(1);
        for (index, client) in self.clients.iter().enumerate() {
            eprintln!("----- bounded client {index} wire frames -----");
            for line in ring_tail(client, client_lines) {
                eprintln!("{line}");
            }
        }
    }
}

pub fn test_root(prefix: &str) -> tempfile::TempDir {
    #[cfg(target_os = "macos")]
    const SHORT_TMP_ROOT: &str = "/private/tmp";
    #[cfg(all(not(target_os = "macos"), unix))]
    const SHORT_TMP_ROOT: &str = "/tmp";

    #[cfg(unix)]
    return tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(SHORT_TMP_ROOT)
        .expect("short temporary root");
    #[cfg(windows)]
    {
        // Hosted runners commonly expose TEMP through an 8.3 alias such as
        // RUNNER~1. The config store path later becomes the hook engine's
        // profile root at its strict canonical-path boundary; preserving that
        // alias manufactures a durable hook_notice in otherwise hook-free
        // tests. Build beneath the canonical base so every derived fixture
        // path is canonical from creation onward.
        let temporary_base =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary base");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(temporary_base)
            .expect("canonical temporary root")
    }
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
    #[cfg(windows)]
    let _trace = install_trace_capture();
    let task = spawn(hermetic(config));
    #[cfg(windows)]
    register_daemon(config, &task);
    await_ready(task).await
}

pub async fn ready_with_dependencies(
    config: &DaemonConfig,
    dependencies: DaemonDependencies,
) -> DaemonTask {
    #[cfg(windows)]
    let _trace = install_trace_capture();
    let task = spawn_with_dependencies(hermetic(config), dependencies);
    #[cfg(windows)]
    register_daemon(config, &task);
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
    pub stream: haider_platform::IpcStream,
    decoder: uds_codec::Decoder,
    pending: VecDeque<WireFrame>,
    #[cfg(windows)]
    frame_limit: usize,
    #[cfg(windows)]
    next_keepalive: tokio::time::Instant,
    #[cfg(windows)]
    history: DiagnosticRing,
    #[cfg(windows)]
    daemon: Option<DaemonTaskDiagnostics>,
}

impl UdsClient {
    pub async fn connect(path: &Path, frame_limit: usize) -> std::io::Result<Self> {
        Ok(Self {
            stream: haider_platform::connect(path).await?,
            decoder: uds_codec::Decoder::new(frame_limit),
            pending: VecDeque::new(),
            #[cfg(windows)]
            frame_limit,
            #[cfg(windows)]
            next_keepalive: tokio::time::Instant::now() + KEEPALIVE_INTERVAL,
            #[cfg(windows)]
            history: Arc::new(StdMutex::new(VecDeque::new())),
            #[cfg(windows)]
            daemon: daemon_diagnostics(path),
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

    /// Windows-only long-wait handshake that reports an EOF instead of
    /// panicking, while preserving the daemon's negotiated-peer keepalive.
    #[cfg(windows)]
    pub async fn try_connect_control_with_keepalive(
        path: &Path,
        frame_limit: usize,
        client_name: &str,
        client_instance_id: &str,
        client_kind: ClientKind,
    ) -> Option<Self> {
        let mut client = Self::connect(path, frame_limit).await.ok()?;
        let hello = WireFrame::Hello(Hello {
            protocol_min: WIRE_PROTOCOL_VERSION,
            protocol_max: WIRE_PROTOCOL_VERSION,
            client_name: client_name.into(),
            client_version: "test".into(),
            client_instance_id: client_instance_id.into(),
            client_kind,
            capabilities_requested: CapabilitySet::from([Capability::View, Capability::Control]),
            max_receive_frame: u32::try_from(frame_limit).expect("frame limit"),
            encodings: Vec::new(),
        });
        if !client.try_send(&hello, frame_limit).await {
            return None;
        }
        match client.try_next_with_keepalive(frame_limit).await {
            Some(WireFrame::Welcome(_)) => Some(client),
            Some(WireFrame::ProtocolError(_)) => None,
            Some(other) => panic!("expected Welcome during reconnect, got {other:?}"),
            None => None,
        }
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
                    encodings: Vec::new(),
                }),
                frame_limit,
            )
            .await;
        assert!(matches!(client.next().await, WireFrame::Welcome(_)));
        client
    }

    pub async fn send(&mut self, frame: &WireFrame, limit: usize) {
        #[cfg(windows)]
        self.record_frame("send", frame);
        let bytes = uds_codec::encode(frame, limit).expect("test frame encodes");
        self.stream.write_all(&bytes).await.expect("frame writes");
    }

    /// Best-effort send for retry loops: a rejected connection may already be
    /// closed by the time the test writes.
    pub async fn try_send(&mut self, frame: &WireFrame, limit: usize) -> bool {
        #[cfg(windows)]
        self.record_frame("send", frame);
        let bytes = uds_codec::encode(frame, limit).expect("test frame encodes");
        self.stream.write_all(&bytes).await.is_ok()
    }

    pub async fn receive(&mut self) -> WireFrame {
        #[cfg(not(windows))]
        {
            self.try_receive()
                .await
                .expect("connection closed before a frame arrived")
        }
        #[cfg(windows)]
        {
            match self.try_receive().await {
                Some(frame) => frame,
                None => {
                    self.report_connection_failure("connection closed before a frame arrived");
                    panic!("connection closed before a frame arrived")
                }
            }
        }
    }

    /// Receives the next request/reply outcome while draining uncorrelated
    /// resident-binding pushes. Tests that assert push delivery or ordering
    /// must keep using [`Self::receive`] (or [`Self::next`]) directly.
    pub async fn receive_reply(&mut self) -> WireFrame {
        loop {
            let frame = self.receive().await;
            if !matches!(frame, WireFrame::ResidentSessionBinding { .. }) {
                return frame;
            }
        }
    }

    pub async fn next(&mut self) -> WireFrame {
        #[cfg(windows)]
        {
            self.next_with_keepalive(self.frame_limit).await
        }
        #[cfg(not(windows))]
        {
            tokio::time::timeout(DEADLINE, self.receive())
                .await
                .expect("frame deadline")
        }
    }

    /// Deadline-bounded counterpart to [`Self::receive_reply`].
    pub async fn next_reply(&mut self) -> WireFrame {
        #[cfg(windows)]
        {
            loop {
                let frame = self.next_with_keepalive(self.frame_limit).await;
                if !matches!(frame, WireFrame::ResidentSessionBinding { .. }) {
                    return frame;
                }
            }
        }
        #[cfg(not(windows))]
        tokio::time::timeout(DEADLINE, self.receive_reply())
            .await
            .expect("reply deadline")
    }

    /// Waits for one frame while preserving the negotiated peer's R9 side of
    /// the liveness contract. Use this only around deliberately slow setup;
    /// tests that exercise silent-peer teardown must keep using `receive`.
    pub async fn next_with_keepalive(&mut self, limit: usize) -> WireFrame {
        #[cfg(windows)]
        {
            tokio::time::timeout(DEADLINE, self.try_next_with_keepalive(limit))
                .await
                .expect("frame deadline")
                .unwrap_or_else(|| {
                    self.report_connection_failure("connection closed during keepalive receive");
                    panic!("connection closed before a frame arrived")
                })
        }
        #[cfg(not(windows))]
        {
            tokio::time::timeout(DEADLINE, async {
                loop {
                    match tokio::time::timeout(KEEPALIVE_INTERVAL, self.receive()).await {
                        Ok(frame) => return frame,
                        Err(_) => {
                            self.send(
                                &WireFrame::Ping {
                                    nonce: u64::MAX - 1,
                                },
                                limit,
                            )
                            .await;
                        }
                    }
                }
            })
            .await
            .expect("frame deadline")
        }
    }

    /// EOF-aware Windows counterpart used inside an independently bounded
    /// process-start wait. It deliberately has no second outer deadline.
    #[cfg(windows)]
    pub async fn try_next_with_keepalive(&mut self, limit: usize) -> Option<WireFrame> {
        loop {
            let now = tokio::time::Instant::now();
            if now >= self.next_keepalive {
                if !self
                    .try_send(
                        &WireFrame::Ping {
                            nonce: u64::MAX - 1,
                        },
                        limit,
                    )
                    .await
                {
                    return None;
                }
                self.next_keepalive = tokio::time::Instant::now() + KEEPALIVE_INTERVAL;
                continue;
            }
            let remaining = self.next_keepalive.saturating_duration_since(now);
            if let Ok(frame) = tokio::time::timeout(remaining, self.try_receive()).await {
                return frame;
            }
        }
    }

    #[cfg(windows)]
    pub fn inherit_diagnostics_from(&mut self, previous: &Self) {
        let current = self.history_snapshot();
        self.history = Arc::clone(&previous.history);
        self.daemon.clone_from(&previous.daemon);
        for line in current {
            push_diagnostic(&self.history, line);
        }
    }

    #[cfg(windows)]
    pub fn history_snapshot(&self) -> Vec<String> {
        ring_snapshot(&self.history)
    }

    #[cfg(windows)]
    fn record_frame(&self, direction: &str, frame: &WireFrame) {
        push_diagnostic(&self.history, format!("{direction}: {frame:?}"));
    }

    #[cfg(windows)]
    pub fn report_connection_failure(&self, reason: &str) {
        eprintln!("{reason}");
        match &self.daemon {
            Some(daemon) => eprintln!(
                "in-process daemon health (no OS exit status): {:?}",
                daemon.snapshot()
            ),
            None => eprintln!("in-process daemon health unavailable for this endpoint"),
        }
        eprintln!("last bounded wire frames:");
        for line in ring_tail(
            &self.history,
            DIAGNOSTIC_LINES.saturating_sub(DIAGNOSTIC_TRACE_PRINT_LINES),
        ) {
            eprintln!("{line}");
        }
        eprintln!("last bounded daemon trace/panic output:");
        for line in ring_tail(&trace_ring(), DIAGNOSTIC_TRACE_PRINT_LINES) {
            eprintln!("{line}");
        }
    }

    /// Next frame, or `None` when the daemon closed the connection first.
    pub async fn try_receive(&mut self) -> Option<WireFrame> {
        if let Some(frame) = self.pending.pop_front() {
            #[cfg(windows)]
            self.record_frame("recv", &frame);
            return Some(frame);
        }
        loop {
            let mut bytes = [0_u8; 16 * 1024];
            #[cfg(not(windows))]
            let read = self.stream.read(&mut bytes).await.expect("frame reads");
            #[cfg(windows)]
            let read = match self.stream.read(&mut bytes).await {
                Ok(read) => read,
                Err(error) => {
                    self.report_connection_failure(&format!("frame read failed: {error}"));
                    panic!("frame reads: {error}")
                }
            };
            if read == 0 {
                return None;
            }
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "server sent an invalid frame");
            self.pending.extend(batch.frames);
            if let Some(frame) = self.pending.pop_front() {
                #[cfg(windows)]
                self.record_frame("recv", &frame);
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
