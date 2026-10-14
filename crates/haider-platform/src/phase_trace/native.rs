use super::{Phase, StoreReadCaller};
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
    store_caller: StoreReadCaller,
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
        records.dropped = records.dropped.saturating_add(1);
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
    state: Option<(Phase, StoreReadCaller, u64, PollCpu, TraceCounters)>,
}

impl Scope {
    pub(super) fn new(phase: Phase) -> Self {
        Self::new_with_store_caller(phase, StoreReadCaller::Unattributed)
    }

    pub(super) fn new_with_store_caller(phase: Phase, store_caller: StoreReadCaller) -> Self {
        Self {
            state: enabled().then(|| {
                (
                    phase,
                    store_caller,
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
        if let Some((_, _, _, _, counters)) = self.state.as_mut() {
            counters.bytes_read = counters
                .bytes_read
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        }
    }

    pub(super) fn note_block_hashed(&mut self) {
        if let Some((_, _, _, _, counters)) = self.state.as_mut() {
            counters.blocks_hashed = counters.blocks_hashed.saturating_add(1);
        }
    }

    pub(super) fn note_reverify_call(&mut self) {
        if let Some((_, _, _, _, counters)) = self.state.as_mut() {
            counters.reverify_calls = counters.reverify_calls.saturating_add(1);
        }
    }

    pub(super) fn note_rows_read(&mut self, rows: usize) {
        if let Some((_, _, _, _, counters)) = self.state.as_mut() {
            counters.rows_read = counters
                .rows_read
                .saturating_add(u64::try_from(rows).unwrap_or(u64::MAX));
        }
    }

    pub(super) fn note_payload_bytes(&mut self, bytes: usize) {
        if let Some((_, _, _, _, counters)) = self.state.as_mut() {
            counters.payload_bytes = counters
                .payload_bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        }
    }

    pub(super) fn note_event_decoded(&mut self) {
        if let Some((_, _, _, _, counters)) = self.state.as_mut() {
            counters.events_decoded = counters.events_decoded.saturating_add(1);
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((phase, store_caller, start, cpu, counters)) = self.state.take() {
            let cpu = cpu.finish();
            record(Row {
                phase,
                store_caller,
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
                store_caller: StoreReadCaller::Unattributed,
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
                store_caller: StoreReadCaller::Unattributed,
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
            "{{\"schema\":4,\"pid\":{},\"dropped\":{},\"records\":{},\"clock\":\"CLOCK_MONOTONIC\",\"cpu_clock\":\"CLOCK_THREAD_CPUTIME_ID\"}}",
            std::process::id(),
            records.dropped,
            records.rows.len()
        )?;
        for row in &records.rows {
            let caller = row.store_caller.name().map_or(String::new(), |name| {
                format!(",\"store_caller\":\"{name}\"")
            });
            writeln!(
                output,
                "{{\"phase\":\"{}\"{},\"start_ns\":{},\"end_ns\":{},\"cpu_ns\":{},\"waiting\":{},\"counters\":{{\"bytes_read\":{},\"blocks_hashed\":{},\"reverify_calls\":{},\"rows_read\":{},\"payload_bytes\":{},\"events_decoded\":{}}}}}",
                row.phase.name(),
                caller,
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
#[path = "native_tests.rs"]
mod tests;
