//! Content-free benchmark scopes, disabled unless `HAIDER_PHASE_TRACE_DIR` is set.
//!
//! CPU is *thread* time during active polls, with nested scopes subtracted.
//! A future may migrate threads or wait without charging unrelated worker CPU.
//! Wall intervals include waits and may overlap; consumers must not sum them.
//! Records are bounded and buffered until the executable's exit guard flushes.
//! Windows currently emits no records: consumers must report unavailable data.

use std::future::Future;

/// Frozen report vocabulary. No caller-supplied text enters a trace.
#[derive(Clone, Copy, Debug)]
pub enum Phase {
    ClientControl,
    TurnControl,
    TurnSetup,
    LockdownBindActivate,
    StoreAccess,
    StoreOther,
    StoreOwnerLockWait,
    StoreConnectionLockWait,
    StoreQueryReplay,
    StoreQueryReducer,
    StoreQueryPoint,
    StoreEventDecode,
    StoreProviderView,
    StoreReceiptAttempt,
    CasReadHash,
    CasReverify,
    Spawn,
    RuntimeInit,
    StoreOpen,
    DirectoryPrep,
    SocketHandshake,
    CapabilityCatalog,
    Submit,
    Rpc,
    StoreJournal,
    ProjectionDigest,
    ProviderAssembly,
    StreamDecode,
    ToolDispatch,
    CompletionRender,
    Teardown,
}

impl Phase {
    #[cfg(unix)]
    fn name(self) -> &'static str {
        match self {
            Self::ClientControl => "client_control",
            Self::TurnControl => "turn_control",
            Self::TurnSetup => "turn_setup",
            Self::LockdownBindActivate => "lockdown_bind_activate",
            Self::StoreAccess => "store_access",
            Self::StoreOther => "store_other",
            Self::StoreOwnerLockWait => "store_owner_lock_wait",
            Self::StoreConnectionLockWait => "store_connection_lock_wait",
            Self::StoreQueryReplay => "store_query_replay",
            Self::StoreQueryReducer => "store_query_reducer",
            Self::StoreQueryPoint => "store_query_point",
            Self::StoreEventDecode => "store_event_decode",
            Self::StoreProviderView => "store_provider_view",
            Self::StoreReceiptAttempt => "store_receipt_attempt",
            Self::CasReadHash => "cas_read_hash",
            Self::CasReverify => "cas_reverify",
            Self::Spawn => "spawn",
            Self::RuntimeInit => "runtime_init",
            Self::StoreOpen => "store_open",
            Self::DirectoryPrep => "directory_prep",
            Self::SocketHandshake => "socket_handshake",
            Self::CapabilityCatalog => "capability_catalog",
            Self::Submit => "submit",
            Self::Rpc => "rpc",
            Self::StoreJournal => "store_journal",
            Self::ProjectionDigest => "projection_digest",
            Self::ProviderAssembly => "provider_assembly",
            Self::StreamDecode => "stream_decode",
            Self::ToolDispatch => "tool_dispatch",
            Self::CompletionRender => "completion_render",
            Self::Teardown => "teardown",
        }
    }
}

/// Synchronous scope guard; intentionally not Send on supported platforms.
/// Do not retain this guard across `.await`; use [`measure`] there.
#[must_use]
pub struct Scope {
    #[cfg(unix)]
    _native: native::Scope,
}

/// Synchronous wait guard with zero attributed CPU.
///
/// Unlike [`Scope`], this marks its interval as waiting so concurrent active
/// work wins wall attribution. It is intended only for blocking lock calls.
#[must_use]
pub struct WaitScope {
    #[cfg(unix)]
    _native: native::WaitScope,
}

/// Trace-only counters for one CAS hash operation.
///
/// The wrapper deliberately exposes no content or artifact identity. When the
/// phase trace is disabled, [`Self::enabled`] is false and the note methods are
/// no-ops, so callers can retain their ordinary uncounted read loop.
#[must_use]
pub struct CasHashScope {
    scope: Scope,
}

impl CasHashScope {
    pub fn enabled(&self) -> bool {
        #[cfg(unix)]
        {
            self.scope._native.enabled()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub fn note_bytes_read(&mut self, bytes: usize) {
        #[cfg(unix)]
        self.scope._native.note_bytes_read(bytes);
        #[cfg(not(unix))]
        let _ = bytes;
    }

    pub fn note_block_hashed(&mut self) {
        #[cfg(unix)]
        self.scope._native.note_block_hashed();
    }

    pub fn note_reverify_call(&mut self) {
        #[cfg(unix)]
        self.scope._native.note_reverify_call();
    }
}

/// Content-free counters for one named store operation.
///
/// Callers compute counts only after checking [`Self::enabled`], keeping the
/// disabled path to the same no-op branch as every other phase probe.
#[must_use]
pub struct StoreScope {
    scope: Scope,
}

impl StoreScope {
    pub fn enabled(&self) -> bool {
        #[cfg(unix)]
        {
            self.scope._native.enabled()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub fn note_rows_read(&mut self, rows: usize) {
        #[cfg(unix)]
        self.scope._native.note_rows_read(rows);
        #[cfg(not(unix))]
        let _ = rows;
    }

    pub fn note_payload_bytes(&mut self, bytes: usize) {
        #[cfg(unix)]
        self.scope._native.note_payload_bytes(bytes);
        #[cfg(not(unix))]
        let _ = bytes;
    }

    pub fn note_event_decoded(&mut self) {
        #[cfg(unix)]
        self.scope._native.note_event_decoded();
    }
}

/// Starts one named store sub-scope. The accepted phases are frozen enum
/// variants, so no SQL, path, identifier, or payload content enters a trace.
pub fn store_scope(phase: Phase) -> StoreScope {
    debug_assert!(matches!(
        phase,
        Phase::StoreOther
            | Phase::StoreOwnerLockWait
            | Phase::StoreConnectionLockWait
            | Phase::StoreQueryReplay
            | Phase::StoreQueryReducer
            | Phase::StoreQueryPoint
            | Phase::StoreEventDecode
            | Phase::StoreProviderView
            | Phase::StoreReceiptAttempt
    ));
    StoreScope {
        scope: scope(phase),
    }
}

/// Starts a synchronous store-lock wait interval.
pub fn store_wait_scope(phase: Phase) -> WaitScope {
    debug_assert!(matches!(
        phase,
        Phase::StoreOwnerLockWait | Phase::StoreConnectionLockWait
    ));
    #[cfg(not(unix))]
    let _ = phase;
    WaitScope {
        #[cfg(unix)]
        _native: native::WaitScope::new(phase),
    }
}

/// Starts a content-free CAS hash scope. Only ordinary read-path verification
/// and write-side re-verification are accepted so their costs stay separable.
pub fn cas_hash_scope(reverify: bool) -> CasHashScope {
    CasHashScope {
        scope: scope(if reverify {
            Phase::CasReverify
        } else {
            Phase::CasReadHash
        }),
    }
}

pub fn scope(phase: Phase) -> Scope {
    #[cfg(not(unix))]
    let _ = phase;
    Scope {
        #[cfg(unix)]
        _native: native::Scope::new(phase),
    }
}

/// Measure a synchronous operation. The closure cannot hold a scope over await.
pub fn sync<T>(phase: Phase, operation: impl FnOnce() -> T) -> T {
    let _scope = scope(phase);
    operation()
}

/// Measure an asynchronous operation, charging CPU only during its polls.
pub fn measure<F: Future>(phase: Phase, future: F) -> impl Future<Output = F::Output> {
    #[cfg(unix)]
    {
        native::Measure::new(phase, future, true)
    }
    #[cfg(not(unix))]
    {
        let _ = phase;
        future
    }
}

/// Attribute active control/handler work without calling its idle time RPC or
/// request assembly. Nested specific scopes retain their own exclusive CPU.
pub fn measure_active<F: Future>(phase: Phase, future: F) -> impl Future<Output = F::Output> {
    #[cfg(unix)]
    {
        native::Measure::new(phase, future, false)
    }
    #[cfg(not(unix))]
    {
        let _ = phase;
        future
    }
}

/// Declare before all executable state so its drop flushes after shutdown.
pub struct ExitGuard;

impl Drop for ExitGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        native::flush();
    }
}

#[cfg(unix)]
mod native {
    use super::Phase;
    use rustix::time::{ClockId, clock_gettime};
    use std::cell::RefCell;
    use std::future::Future;
    use std::io::{BufWriter, Write};
    use std::marker::PhantomData;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::{Mutex, OnceLock};
    use std::task::{Context, Poll};

    const MAX_RECORDS: usize = 100_000;
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    static RECORDS: Mutex<Records> = Mutex::new(Records {
        rows: Vec::new(),
        dropped: 0,
    });
    thread_local! {
        static CHILD_CPU: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn enabled() -> bool {
        DIRECTORY
            .get_or_init(|| {
                std::env::var_os("HAIDER_PHASE_TRACE_DIR")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .is_some()
    }

    fn clock_ns(clock: ClockId) -> u64 {
        let time = clock_gettime(clock);
        u64::try_from(time.tv_sec).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec).unwrap_or(0)
    }

    struct Row {
        phase: Phase,
        start: u64,
        end: u64,
        cpu: u64,
        waiting: bool,
        counters: TraceCounters,
    }

    #[derive(Clone, Copy, Default)]
    struct TraceCounters {
        bytes_read: u64,
        blocks_hashed: u64,
        reverify_calls: u64,
        rows_read: u64,
        payload_bytes: u64,
        events_decoded: u64,
    }
    struct Records {
        rows: Vec<Row>,
        dropped: usize,
    }

    fn record(row: Row) {
        let mut records = RECORDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if records.rows.len() < MAX_RECORDS {
            records.rows.push(row);
        } else {
            records.dropped += 1;
        }
    }

    // Never Send: enter/leave must execute on the same thread. Async scopes
    // create this guard inside each poll, not across suspension.
    struct PollCpu {
        start: u64,
        _same_thread: PhantomData<Rc<()>>,
    }

    impl PollCpu {
        fn new() -> Self {
            CHILD_CPU.with(|stack| stack.borrow_mut().push(0));
            Self {
                start: clock_ns(ClockId::ThreadCPUTime),
                _same_thread: PhantomData,
            }
        }

        fn finish(self) -> u64 {
            let inclusive = clock_ns(ClockId::ThreadCPUTime) - self.start;
            CHILD_CPU.with(|stack| {
                let mut stack = stack.borrow_mut();
                let children = stack.pop().unwrap_or(0);
                if let Some(parent) = stack.last_mut() {
                    *parent += inclusive;
                }
                assert!(inclusive >= children, "nested phase CPU exceeds parent");
                inclusive - children
            })
        }
    }

    pub(super) struct Scope {
        state: Option<(Phase, u64, PollCpu, TraceCounters)>,
    }

    impl Scope {
        pub(super) fn new(phase: Phase) -> Self {
            Self {
                state: enabled().then(|| {
                    (
                        phase,
                        clock_ns(ClockId::Monotonic),
                        PollCpu::new(),
                        TraceCounters::default(),
                    )
                }),
            }
        }

        pub(super) fn enabled(&self) -> bool {
            self.state.is_some()
        }

        pub(super) fn note_bytes_read(&mut self, bytes: usize) {
            if let Some((_, _, _, counters)) = self.state.as_mut() {
                counters.bytes_read = counters
                    .bytes_read
                    .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
            }
        }

        pub(super) fn note_block_hashed(&mut self) {
            if let Some((_, _, _, counters)) = self.state.as_mut() {
                counters.blocks_hashed = counters.blocks_hashed.saturating_add(1);
            }
        }

        pub(super) fn note_reverify_call(&mut self) {
            if let Some((_, _, _, counters)) = self.state.as_mut() {
                counters.reverify_calls = counters.reverify_calls.saturating_add(1);
            }
        }

        pub(super) fn note_rows_read(&mut self, rows: usize) {
            if let Some((_, _, _, counters)) = self.state.as_mut() {
                counters.rows_read = counters
                    .rows_read
                    .saturating_add(u64::try_from(rows).unwrap_or(u64::MAX));
            }
        }

        pub(super) fn note_payload_bytes(&mut self, bytes: usize) {
            if let Some((_, _, _, counters)) = self.state.as_mut() {
                counters.payload_bytes = counters
                    .payload_bytes
                    .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
            }
        }

        pub(super) fn note_event_decoded(&mut self) {
            if let Some((_, _, _, counters)) = self.state.as_mut() {
                counters.events_decoded = counters.events_decoded.saturating_add(1);
            }
        }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            if let Some((phase, start, cpu, counters)) = self.state.take() {
                let cpu = cpu.finish();
                record(Row {
                    phase,
                    start,
                    end: clock_ns(ClockId::Monotonic),
                    cpu,
                    waiting: false,
                    counters,
                });
            }
        }
    }

    pub(super) struct WaitScope {
        state: Option<(Phase, u64)>,
    }

    impl WaitScope {
        pub(super) fn new(phase: Phase) -> Self {
            Self {
                state: enabled().then(|| (phase, clock_ns(ClockId::Monotonic))),
            }
        }
    }

    impl Drop for WaitScope {
        fn drop(&mut self) {
            if let Some((phase, start)) = self.state.take() {
                record(Row {
                    phase,
                    start,
                    end: clock_ns(ClockId::Monotonic),
                    cpu: 0,
                    waiting: true,
                    counters: TraceCounters::default(),
                });
            }
        }
    }

    pub(super) struct Measure<F> {
        phase: Phase,
        future: Pin<Box<F>>,
        record_waits: bool,
        waiting_since: Option<u64>,
        enabled: bool,
    }

    impl<F> Measure<F> {
        pub(super) fn new(phase: Phase, future: F, record_waits: bool) -> Self {
            Self {
                phase,
                future: Box::pin(future),
                record_waits,
                waiting_since: None,
                enabled: enabled(),
            }
        }
    }

    impl<F: Future> Future for Measure<F> {
        type Output = F::Output;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            if !this.enabled {
                return this.future.as_mut().poll(cx);
            }
            if let Some(start) = this.waiting_since.take() {
                record(Row {
                    phase: this.phase,
                    start,
                    end: clock_ns(ClockId::Monotonic),
                    cpu: 0,
                    waiting: true,
                    counters: TraceCounters::default(),
                });
            }
            let scope = Scope::new(this.phase);
            let result = this.future.as_mut().poll(cx);
            drop(scope);
            if this.record_waits && result.is_pending() {
                this.waiting_since = Some(clock_ns(ClockId::Monotonic));
            }
            result
        }
    }

    pub(super) fn flush() {
        if !enabled() {
            return;
        }
        let Some(Some(directory)) = DIRECTORY.get() else {
            return;
        };
        let result = (|| -> std::io::Result<()> {
            // Directory is created/owned by the benchmark, never the product.
            let path = directory.join(format!("phase-{}.jsonl", std::process::id()));
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            let mut output = BufWriter::new(file);
            let records = RECORDS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            writeln!(
                output,
                "{{\"schema\":3,\"pid\":{},\"dropped\":{},\"records\":{},\"clock\":\"CLOCK_MONOTONIC\",\"cpu_clock\":\"CLOCK_THREAD_CPUTIME_ID\"}}",
                std::process::id(),
                records.dropped,
                records.rows.len()
            )?;
            for row in &records.rows {
                writeln!(
                    output,
                    "{{\"phase\":\"{}\",\"start_ns\":{},\"end_ns\":{},\"cpu_ns\":{},\"waiting\":{},\"counters\":{{\"bytes_read\":{},\"blocks_hashed\":{},\"reverify_calls\":{},\"rows_read\":{},\"payload_bytes\":{},\"events_decoded\":{}}}}}",
                    row.phase.name(),
                    row.start,
                    row.end,
                    row.cpu,
                    row.waiting,
                    row.counters.bytes_read,
                    row.counters.blocks_hashed,
                    row.counters.reverify_calls,
                    row.counters.rows_read,
                    row.counters.payload_bytes,
                    row.counters.events_decoded,
                )?;
            }
            output.flush()
        })();
        if result.is_err() {
            eprintln!("haider: phase trace flush failed; attribution is incomplete");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::future::poll_fn;
        use std::pin::pin;

        #[test]
        fn suspended_future_does_not_charge_interleaved_work() {
            const CHILD: &str = "HAIDER_PHASE_TEST_CHILD";
            if std::env::var_os(CHILD).is_none() {
                let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
                let output = std::process::Command::new(
                    std::env::current_exe().unwrap_or_else(|error| panic!("{error}")),
                )
                .args([
                    "--exact",
                    "phase_trace::native::tests::suspended_future_does_not_charge_interleaved_work",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HAIDER_PHASE_TRACE_DIR", directory.path())
                .output()
                .unwrap_or_else(|error| panic!("{error}"));
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let paths = std::fs::read_dir(directory.path())
                    .unwrap_or_else(|error| panic!("{error}"))
                    .map(|entry| entry.unwrap_or_else(|error| panic!("{error}")).path())
                    .collect::<Vec<_>>();
                assert_eq!(paths.len(), 1);
                let trace =
                    std::fs::read_to_string(&paths[0]).unwrap_or_else(|error| panic!("{error}"));
                assert!(trace.contains("\"schema\":3"));
                assert!(trace.contains(
                    "\"counters\":{\"bytes_read\":17,\"blocks_hashed\":1,\"reverify_calls\":1,\"rows_read\":0,\"payload_bytes\":0,\"events_decoded\":0}"
                ));
                assert!(trace.lines().any(|line| {
                    line.contains("\"phase\":\"store_owner_lock_wait\"")
                        && line.contains("\"cpu_ns\":0")
                        && line.contains("\"waiting\":true")
                }));
                return;
            }
            {
                let mut cas = super::super::cas_hash_scope(true);
                cas.note_bytes_read(17);
                cas.note_block_hashed();
                cas.note_reverify_call();
            }
            drop(super::super::store_wait_scope(Phase::StoreOwnerLockWait));
            let mut polled = false;
            let mut future = pin!(Measure::new(
                Phase::Rpc,
                poll_fn(|_| {
                    if std::mem::replace(&mut polled, true) {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                }),
                true,
            ));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(future.as_mut().poll(&mut context).is_pending());
            let start = clock_ns(ClockId::ThreadCPUTime);
            while clock_ns(ClockId::ThreadCPUTime) - start < 10_000_000 {
                std::hint::spin_loop();
            }
            assert!(future.as_mut().poll(&mut context).is_ready());
            {
                let records = RECORDS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                assert!(
                    records
                        .rows
                        .iter()
                        .any(|row| row.waiting && row.end - row.start >= 10_000_000)
                );
                assert!(records.rows.iter().map(|row| row.cpu).sum::<u64>() < 5_000_000);
            }
            flush();
        }

        #[test]
        fn nested_cpu_is_exclusive_and_thread_time_does_not_charge_sleep() {
            let outer = PollCpu::new();
            let inner = PollCpu::new();
            let start = clock_ns(ClockId::ThreadCPUTime);
            while clock_ns(ClockId::ThreadCPUTime) - start < 1_000_000 {
                std::hint::spin_loop();
            }
            let inner_cpu = inner.finish();
            std::thread::sleep(std::time::Duration::from_millis(10));
            let outer_cpu = outer.finish();
            assert!(inner_cpu >= 1_000_000);
            assert!(outer_cpu < 5_000_000, "sleep must not be charged as CPU");
            CHILD_CPU.with(|stack| assert!(stack.borrow().is_empty()));
        }
    }
}
