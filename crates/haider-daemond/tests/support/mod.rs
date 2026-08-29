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

use haider_daemon::DaemonTaskDiagnostics;
use haider_daemon::{
    DaemonConfig, DaemonDependencies, DaemonState, DaemonTask, spawn, spawn_with_dependencies,
};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, Hello, WIRE_PROTOCOL_VERSION, WireFrame, uds_codec,
};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Boundary diagnostics must not replace the intended timeout panic if CI's
// capture pipe is already closed. These local macros preserve the familiar
// call sites while making every stdout write explicitly best effort.
macro_rules! println {
    () => {{
        let _ = std::io::Write::write_all(&mut std::io::stdout().lock(), b"\n");
    }};
    ($($argument:tt)*) => {{
        let _ = std::io::Write::write_fmt(
            &mut std::io::stdout().lock(),
            format_args!("{}\n", format_args!($($argument)*)),
        );
    }};
}

macro_rules! print {
    ($($argument:tt)*) => {{
        let _ = std::io::Write::write_fmt(
            &mut std::io::stdout().lock(),
            format_args!($($argument)*),
        );
    }};
}

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
const KEEPALIVE_NONCE: u64 = u64::MAX - 1;

/// Filesystem coordinates whose failure-only state explains a daemon boundary.
///
/// The successful polling path retains only these borrowed paths. Log reads,
/// endpoint probes, directory walks, and process snapshots happen only after
/// the deadline has elapsed.
pub struct BoundaryContext<'a> {
    pub store_dir: &'a Path,
    pub runtime_dir: &'a Path,
    pub endpoint_path: &'a Path,
    /// Extra fixture-owned daemon stderr captures, if a suite pipes stderr
    /// instead of using the ordinary per-process daemon log.
    pub captured_daemon_stderr: &'a [PathBuf],
}

/// Named predicate state and process state gathered once, at timeout.
#[derive(Default)]
pub struct BoundarySnapshot {
    observations: Vec<(String, String)>,
    processes: Vec<(String, String)>,
}

impl BoundarySnapshot {
    #[must_use]
    pub fn observation(mut self, name: impl Into<String>, value: impl ToString) -> Self {
        self.observations.push((name.into(), value.to_string()));
        self
    }

    #[must_use]
    pub fn process(mut self, role: impl Into<String>, status: impl Into<String>) -> Self {
        self.processes.push((role.into(), status.into()));
        self
    }
}

/// Polls a synchronous boundary and prints a complete stdout post-mortem
/// before failing it. `snapshot` is deliberately lazy: success does no I/O,
/// tree walking, endpoint probing, allocation, or process inspection. The
/// delay is driven by the caller's runtime so cancelled Tokio I/O reaches the
/// platform driver before the predicate is checked again.
pub fn wait_until(
    runtime: &tokio::runtime::Runtime,
    boundary: &str,
    deadline: Duration,
    poll: Duration,
    context: &BoundaryContext<'_>,
    mut predicate: impl FnMut() -> bool,
    mut snapshot: impl FnMut() -> BoundarySnapshot,
) {
    let started = std::time::Instant::now();
    let deadline_at = started + deadline;
    loop {
        if predicate() {
            return;
        }
        let now = std::time::Instant::now();
        if now >= deadline_at {
            report_boundary_timeout(boundary, started.elapsed(), deadline, context, snapshot());
            panic!("deadline waiting for {boundary}");
        }
        let delay = poll.min(deadline_at.saturating_duration_since(now));
        runtime.block_on(async { tokio::time::sleep(delay).await });
    }
}

/// Failure-only reporter shared by synchronous polls and async timeout arms.
/// Every fallible diagnostic operation is rendered as data; none can replace
/// the boundary failure with a secondary filesystem or process panic.
pub fn report_boundary_timeout(
    boundary: &str,
    elapsed: Duration,
    deadline: Duration,
    context: &BoundaryContext<'_>,
    snapshot: BoundarySnapshot,
) {
    report_boundary_failure_inner(boundary, elapsed, Some(deadline), context, snapshot);
}

/// Prints the same complete, best-effort post-mortem for an assertion or
/// terminal-state failure whose boundary was not itself a timeout.
pub fn report_boundary_failure(
    boundary: &str,
    elapsed: Duration,
    context: &BoundaryContext<'_>,
    snapshot: BoundarySnapshot,
) {
    report_boundary_failure_inner(boundary, elapsed, None, context, snapshot);
}

fn report_boundary_failure_inner(
    boundary: &str,
    elapsed: Duration,
    deadline: Option<Duration>,
    context: &BoundaryContext<'_>,
    snapshot: BoundarySnapshot,
) {
    println!("===== haider boundary post-mortem =====");
    println!("boundary={boundary}");
    match deadline {
        Some(deadline) => println!(
            "timing elapsed_ms={} deadline_ms={}",
            elapsed.as_millis(),
            deadline.as_millis()
        ),
        None => println!("timing elapsed_ms={}", elapsed.as_millis()),
    }
    if deadline.is_some() {
        println!("----- predicate state (re-evaluated at timeout) -----");
    } else {
        println!("----- predicate state (captured at failure) -----");
    }
    if snapshot.observations.is_empty() {
        println!("observation_state=unavailable");
    } else {
        for (name, value) in snapshot.observations {
            println!("{name}={value}");
        }
    }
    println!("----- process state -----");
    if snapshot.processes.is_empty() {
        println!("process_state=unavailable");
    } else {
        for (role, status) in snapshot.processes {
            println!("process role={role} status={status}");
        }
    }
    // Freeze existing evidence before the endpoint probe opens a real stream
    // and can thereby create diagnostic admission/retirement journal lines.
    print_daemon_logs(context.store_dir, context.captured_daemon_stderr);
    print_runtime_tree(context.runtime_dir);
    print_endpoint_state(context.endpoint_path);
    println!("===== end haider boundary post-mortem =====");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn print_endpoint_state(endpoint_path: &Path) {
    println!("----- endpoint state -----");
    println!("endpoint_path={}", endpoint_path.display());
    #[cfg(unix)]
    println!("unix_socket_path_exists={}", endpoint_path.exists());

    let endpoint = endpoint_path.to_path_buf();
    let probe = std::thread::Builder::new()
        .name("haider-boundary-endpoint-probe".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| (format!("runtime_error:{error}"), None))
                .and_then(|runtime| {
                    runtime
                        .block_on(async {
                            tokio::time::timeout(
                                Duration::from_millis(500),
                                haider_platform::connect(endpoint),
                            )
                            .await
                        })
                        .map_err(|_| ("probe_timeout".to_owned(), None))
                        .and_then(|result| {
                            result.map_err(|error| {
                                let raw_os_error = error.raw_os_error();
                                (format!("connect_error:{error}"), raw_os_error)
                            })
                        })
                        .map(drop)
                })
        })
        .map_err(|error| (format!("probe_thread_error:{error}"), None))
        .and_then(|thread| {
            thread
                .join()
                .map_err(|_| ("probe_thread_panicked".to_owned(), None))?
        });
    println!("endpoint_reachable={}", probe.is_ok());
    if let Err((error, raw_os_error)) = &probe {
        println!("endpoint_probe={error}");
        println!("endpoint_probe_raw_os_error={raw_os_error:?}");
    }
    #[cfg(windows)]
    {
        // A successful open proves existence. ERROR_PIPE_BUSY (231) also
        // proves a name exists even when every instance is occupied. Other
        // errors remain explicit instead of manufacturing a false answer.
        let exists = match &probe {
            Ok(()) => "true".to_owned(),
            Err((_, Some(231))) => "true (all instances busy)".into(),
            Err((_, Some(5))) => "true (access denied)".into(),
            Err((_, Some(2 | 3))) => "false".into(),
            Err((error, _)) => format!("unknown ({error})"),
        };
        println!("named_pipe_exists={exists}");
    }
}

fn print_runtime_tree(runtime_dir: &Path) {
    println!("----- runtime directory tree -----");
    match std::fs::symlink_metadata(runtime_dir) {
        Ok(metadata) => {
            print_tree_entry(runtime_dir, &metadata);
            let mut pending = vec![runtime_dir.to_path_buf()];
            while let Some(directory) = pending.pop() {
                let entries = match std::fs::read_dir(&directory) {
                    Ok(entries) => entries,
                    Err(error) => {
                        println!(
                            "tree_error path={} operation=read_dir error={error}",
                            directory.display()
                        );
                        continue;
                    }
                };
                let mut paths = entries
                    .filter_map(|entry| match entry {
                        Ok(entry) => Some(entry.path()),
                        Err(error) => {
                            println!(
                                "tree_error path={} operation=read_entry error={error}",
                                directory.display()
                            );
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                paths.sort();
                for path in paths.into_iter().rev() {
                    match std::fs::symlink_metadata(&path) {
                        Ok(metadata) => {
                            print_tree_entry(&path, &metadata);
                            if metadata.is_dir() {
                                pending.push(path);
                            }
                        }
                        Err(error) => println!(
                            "tree_error path={} operation=metadata error={error}",
                            path.display()
                        ),
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("runtime_dir_exists=false path={}", runtime_dir.display());
        }
        Err(error) => println!(
            "tree_error path={} operation=root_metadata error={error}",
            runtime_dir.display()
        ),
    }
}

fn print_tree_entry(path: &Path, metadata: &std::fs::Metadata) {
    let kind = if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "other"
    };
    println!(
        "tree_entry path={} kind={kind} size_bytes={}",
        path.display(),
        metadata.len()
    );
}

fn print_daemon_logs(store_dir: &Path, captured_stderr: &[PathBuf]) {
    println!("----- daemon diagnostic logs (full contents) -----");
    let directory = store_dir.join(haider_platform::DAEMON_LOG_DIRECTORY);
    let mut logs = Vec::new();
    match std::fs::read_dir(&directory) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        let path = entry.path();
                        if path.extension().is_some_and(|extension| extension == "log") {
                            logs.push(path);
                        }
                    }
                    Err(error) => println!(
                        "log_error path={} operation=read_entry error={error}",
                        directory.display()
                    ),
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "daemon_log_directory_exists=false path={}",
                directory.display()
            );
        }
        Err(error) => println!(
            "log_error path={} operation=read_dir error={error}",
            directory.display()
        ),
    }
    let active = store_dir.join(haider_platform::DAEMON_LOG_FILE);
    if active.exists() {
        logs.push(active);
    }
    logs.sort();
    logs.dedup();
    for path in logs {
        print_full_file("daemon_log", &path);
    }
    for path in captured_stderr {
        print_full_file("captured_daemon_stderr", path);
    }
}

fn print_full_file(kind: &str, path: &Path) {
    match std::fs::read(path) {
        Ok(bytes) => {
            println!(
                "{kind}_begin path={} size_bytes={}",
                path.display(),
                bytes.len()
            );
            print!("{}", String::from_utf8_lossy(&bytes));
            if !bytes.ends_with(b"\n") {
                println!();
            }
            println!("{kind}_end path={}", path.display());
        }
        Err(error) => println!(
            "log_error path={} operation=read_file error={error}",
            path.display()
        ),
    }
}

const DIAGNOSTIC_LINES: usize = 200;
const DIAGNOSTIC_LINE_BYTES: usize = 4 * 1024;
const DIAGNOSTIC_TRACE_PRINT_LINES: usize = 50;

type DiagnosticRing = Arc<StdMutex<VecDeque<String>>>;

fn daemon_tasks() -> &'static StdMutex<HashMap<PathBuf, DaemonTaskDiagnostics>> {
    static TASKS: OnceLock<StdMutex<HashMap<PathBuf, DaemonTaskDiagnostics>>> = OnceLock::new();
    TASKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn register_daemon(config: &DaemonConfig, task: &DaemonTask) {
    daemon_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(config.endpoint_path(), task.diagnostics());
}

fn daemon_diagnostics(path: &Path) -> Option<DaemonTaskDiagnostics> {
    daemon_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .cloned()
}

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

fn ring_snapshot(ring: &DiagnosticRing) -> Vec<String> {
    ring.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

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

fn trace_ring() -> DiagnosticRing {
    static TRACE: OnceLock<DiagnosticRing> = OnceLock::new();
    Arc::clone(TRACE.get_or_init(|| Arc::new(StdMutex::new(VecDeque::new()))))
}

fn trace_observers() -> &'static StdMutex<Vec<Weak<StdMutex<String>>>> {
    static OBSERVERS: OnceLock<StdMutex<Vec<Weak<StdMutex<String>>>>> = OnceLock::new();
    OBSERVERS.get_or_init(|| StdMutex::new(Vec::new()))
}

fn push_trace(ring: &DiagnosticRing, line: String) {
    trace_observers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|observer| {
            let Some(output) = observer.upgrade() else {
                return false;
            };
            output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_str(&line);
            true
        });
    push_diagnostic(ring, line);
}

struct TraceFields<'a>(&'a mut String);

impl tracing::field::Visit for TraceFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?};", field.name());
    }
}

struct TraceCapture {
    output: DiagnosticRing,
    next_span: AtomicU64,
}

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
        push_trace(&self.output, line);
        tracing::span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed).max(1))
    }

    fn record(&self, _span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::from("span-record ");
        values.record(&mut TraceFields(&mut line));
        push_trace(&self.output, line);
    }

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!(
            "event target={};name={};",
            event.metadata().target(),
            event.metadata().name()
        );
        event.record(&mut TraceFields(&mut line));
        push_trace(&self.output, line);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

fn trace_install_state() -> &'static OnceLock<bool> {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    &INSTALLED
}

fn install_trace_capture() -> DiagnosticRing {
    let output = trace_ring();
    trace_install_state().get_or_init(|| {
        let installed = tracing::subscriber::set_global_default(TraceCapture {
            output: Arc::clone(&output),
            next_span: AtomicU64::new(1),
        })
        .is_ok();
        if !installed {
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
        installed
    });
    output
}

/// Captures the complete tracing stream while the returned observer is alive.
/// The shared harness subscriber remains the sole process-global owner, so
/// parallel tests cannot race to replace it.
pub fn capture_trace_output() -> Arc<StdMutex<String>> {
    let output = Arc::new(StdMutex::new(String::new()));
    trace_observers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Arc::downgrade(&output));
    let _trace = install_trace_capture();
    assert!(
        *trace_install_state()
            .get()
            .expect("trace installation state"),
        "install tracing capture"
    );
    output
}

/// Prints the bounded in-process daemon trace and wire transcript when a test
/// unwinds. There is no child stdout/stderr or OS ExitStatus in this harness;
/// the trace/panic ring and typed daemon-task outcome are the truthful
/// equivalents on every platform.
pub struct FailureDiagnostics {
    label: &'static str,
    daemon: DaemonTaskDiagnostics,
    trace: DiagnosticRing,
    clients: Vec<DiagnosticRing>,
}

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

impl Drop for FailureDiagnostics {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        eprintln!("===== {} daemon failure diagnostics =====", self.label);
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
    {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(SHORT_TMP_ROOT)
            .expect("short temporary root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("owner-private temporary root");
        root
    }
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
    config.lockdown_root_override = Some(hermetic_machine_home().join(".haider").join("lockdown"));
    config
}

fn hermetic_machine_home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| test_root("haider-machine-home-"))
        .path()
}

pub async fn ready(config: &DaemonConfig) -> DaemonTask {
    let _trace = install_trace_capture();
    let task = spawn(hermetic(config));
    register_daemon(config, &task);
    await_ready(task).await
}

pub async fn ready_with_dependencies(
    config: &DaemonConfig,
    dependencies: DaemonDependencies,
) -> DaemonTask {
    let _trace = install_trace_capture();
    let task = spawn_with_dependencies(hermetic(config), dependencies);
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
    frame_limit: usize,
    next_keepalive: tokio::time::Instant,
    history: DiagnosticRing,
    daemon: Option<DaemonTaskDiagnostics>,
}

impl UdsClient {
    pub async fn connect(path: &Path, frame_limit: usize) -> std::io::Result<Self> {
        Ok(Self {
            stream: haider_platform::connect(path).await?,
            decoder: uds_codec::Decoder::new(frame_limit),
            pending: VecDeque::new(),
            frame_limit,
            next_keepalive: tokio::time::Instant::now() + KEEPALIVE_INTERVAL,
            history: Arc::new(StdMutex::new(VecDeque::new())),
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

    /// Long-wait handshake that reports an EOF instead of panicking, while
    /// preserving the daemon's negotiated-peer keepalive.
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
        self.record_frame("send", frame);
        let bytes = uds_codec::encode(frame, limit).expect("test frame encodes");
        self.stream.write_all(&bytes).await.expect("frame writes");
    }

    /// Best-effort send for retry loops: a rejected connection may already be
    /// closed by the time the test writes.
    pub async fn try_send(&mut self, frame: &WireFrame, limit: usize) -> bool {
        self.record_frame("send", frame);
        let bytes = uds_codec::encode(frame, limit).expect("test frame encodes");
        self.stream.write_all(&bytes).await.is_ok()
    }

    pub async fn receive(&mut self) -> WireFrame {
        match self.try_receive().await {
            Some(frame) => frame,
            None => {
                self.report_connection_failure("connection closed before a frame arrived");
                panic!("connection closed before a frame arrived")
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
        self.next_with_keepalive(self.frame_limit).await
    }

    /// Deadline-bounded counterpart to [`Self::receive_reply`].
    pub async fn next_reply(&mut self) -> WireFrame {
        loop {
            let frame = self.next_with_keepalive(self.frame_limit).await;
            if !matches!(frame, WireFrame::ResidentSessionBinding { .. }) {
                return frame;
            }
        }
    }

    /// Waits for one frame while preserving the negotiated peer's R9 side of
    /// the liveness contract. Use this only around deliberately slow setup;
    /// tests that exercise silent-peer teardown must keep using `receive`.
    pub async fn next_with_keepalive(&mut self, limit: usize) -> WireFrame {
        #[cfg(windows)]
        {
            // Keep the win2 contract from ee2b1ce verbatim: a reserved Pong is
            // a received frame on Windows. Returning it keeps repeated calls
            // alive while their caller owns the one continuous long-wait
            // deadline; swallowing it here reintroduces a per-frame deadline.
            tokio::time::timeout(DEADLINE, self.try_next_with_keepalive(limit))
                .await
                .expect("frame deadline")
                .unwrap_or_else(|| {
                    self.report_connection_failure("connection closed during keepalive receive");
                    panic!("connection closed before a frame arrived")
                })
        }
        #[cfg(not(windows))]
        match tokio::time::timeout(DEADLINE, self.try_next_with_keepalive(limit)).await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                self.report_connection_failure("connection closed during keepalive receive");
                panic!("connection closed before a frame arrived")
            }
            Err(_) => {
                self.report_connection_failure("frame deadline elapsed during keepalive receive");
                panic!("frame deadline")
            }
        }
    }

    /// EOF-aware counterpart used inside an independently bounded long wait.
    /// It deliberately has no second outer deadline.
    pub async fn try_next_with_keepalive(&mut self, limit: usize) -> Option<WireFrame> {
        loop {
            let now = tokio::time::Instant::now();
            if now >= self.next_keepalive {
                if !self
                    .try_send(
                        &WireFrame::Ping {
                            nonce: KEEPALIVE_NONCE,
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
            #[cfg(windows)]
            if let Ok(frame) = tokio::time::timeout(remaining, self.try_receive()).await {
                // ee2b1ce intentionally returns the reserved keepalive Pong:
                // the Windows caller, not this per-frame helper, owns the
                // continuous operation deadline and EOF reconnect policy.
                return frame;
            }
            #[cfg(not(windows))]
            match tokio::time::timeout(remaining, self.try_receive()).await {
                // Transport-control replies are not test observations. Keeping
                // this reserved Pong inside the same call also preserves the
                // caller's one continuous outer frame deadline.
                Ok(Some(WireFrame::Pong { nonce })) if nonce == KEEPALIVE_NONCE => {}
                Ok(frame) => return frame,
                Err(_) => {}
            }
        }
    }

    pub fn inherit_diagnostics_from(&mut self, previous: &Self) {
        let current = self.history_snapshot();
        self.history = Arc::clone(&previous.history);
        self.daemon.clone_from(&previous.daemon);
        for line in current {
            push_diagnostic(&self.history, line);
        }
    }

    pub fn history_snapshot(&self) -> Vec<String> {
        ring_snapshot(&self.history)
    }

    fn record_frame(&self, direction: &str, frame: &WireFrame) {
        push_diagnostic(&self.history, format!("{direction}: {frame:?}"));
    }

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
            self.record_frame("recv", &frame);
            return Some(frame);
        }
        loop {
            let mut bytes = [0_u8; 16 * 1024];
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
