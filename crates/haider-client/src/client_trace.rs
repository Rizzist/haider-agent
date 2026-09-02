//! Content-free timing for the one-shot client's process envelope.
//!
//! The trace object is never constructed unless the shared daemon-trace gate
//! is enabled. Normal clients therefore pay no allocation and read no clock.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

const PROFILE_RESOLVED: usize = 0;
const CONNECTED: usize = 1;
const HELLO_DONE: usize = 2;
const SUBMITTED: usize = 3;
const TERMINAL_SEEN: usize = 4;
const MARKER_COUNT: usize = 5;

/// One successful `haider run` process envelope, joined to the daemon by the
/// content-free accepted-turn ordinal.
#[derive(Debug)]
pub struct ClientEnvelopeTrace {
    exec_started: Instant,
    /// Stored as elapsed microseconds plus one, reserving zero for "unseen".
    markers: [AtomicU64; MARKER_COUNT],
    turn_ordinal: AtomicU64,
    emitted: AtomicBool,
}

impl ClientEnvelopeTrace {
    /// Constructs the trace only behind the caller's cached shared gate.
    #[must_use]
    pub fn new_if_enabled(enabled: bool) -> Option<Arc<Self>> {
        Self::new_if_enabled_with_clock(enabled, Instant::now)
    }

    fn new_if_enabled_with_clock(
        enabled: bool,
        clock: impl FnOnce() -> Instant,
    ) -> Option<Arc<Self>> {
        enabled.then(|| {
            Arc::new(Self {
                exec_started: clock(),
                markers: std::array::from_fn(|_| AtomicU64::new(0)),
                turn_ordinal: AtomicU64::new(0),
                emitted: AtomicBool::new(false),
            })
        })
    }

    pub fn profile_resolved(&self) {
        self.mark(PROFILE_RESOLVED);
    }

    pub fn connected(&self) {
        self.mark(CONNECTED);
    }

    pub fn hello_done(&self) {
        self.mark(HELLO_DONE);
    }

    pub fn submitted(&self) {
        self.mark(SUBMITTED);
    }

    pub fn terminal_seen(&self) {
        self.mark(TERMINAL_SEEN);
    }

    /// Binds the client clock to the daemon's stable accepted-turn identity.
    /// Reconnect/replay announcements cannot replace the first binding.
    pub fn bind_turn_ordinal(&self, ordinal: u64) {
        if ordinal > 0 {
            let _ = self.turn_ordinal.compare_exchange(
                0,
                ordinal,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }

    /// Content-free state exposed for cross-crate wiring tests.
    #[doc(hidden)]
    #[must_use]
    pub fn audit_snapshot(&self) -> (u64, bool) {
        (
            self.turn_ordinal.load(Ordering::Relaxed),
            self.marker_us(TERMINAL_SEEN).is_some(),
        )
    }

    fn mark(&self, marker: usize) {
        self.mark_at_us(
            marker,
            u64::try_from(self.exec_started.elapsed().as_micros()).unwrap_or(u64::MAX),
        );
    }

    fn mark_at_us(&self, marker: usize, elapsed_us: u64) {
        let encoded = elapsed_us.saturating_add(1);
        let _ =
            self.markers[marker].compare_exchange(0, encoded, Ordering::Relaxed, Ordering::Relaxed);
    }

    fn marker_us(&self, marker: usize) -> Option<u64> {
        self.markers[marker].load(Ordering::Relaxed).checked_sub(1)
    }

    /// Records the late client exit seam after the async runtime is dropped,
    /// then writes every phase in one stderr operation. The timestamp is taken
    /// before formatting/I/O so trace publication remains in the derived
    /// process residual rather than contaminating a measured client phase.
    pub fn emit_exit(&self) {
        let end_us = u64::try_from(self.exec_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let rendered = self.render_at_us(end_us);
        if !rendered.is_empty() {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(rendered.as_bytes());
            let _ = stderr.flush();
        }
    }

    fn render_at_us(&self, exit_us: u64) -> String {
        if self.emitted.swap(true, Ordering::Relaxed) {
            return String::new();
        }
        let turn_ordinal = self.turn_ordinal.load(Ordering::Relaxed);
        if turn_ordinal == 0 {
            return String::new();
        }
        let mut rendered = String::new();
        Self::write_record(&mut rendered, "exec_start", turn_ordinal, 0, 0);
        let mut start = 0;
        for (marker, phase) in [
            (PROFILE_RESOLVED, "profile_resolved"),
            (CONNECTED, "connected"),
            (HELLO_DONE, "hello_done"),
            (SUBMITTED, "submitted"),
            (TERMINAL_SEEN, "terminal_seen"),
        ] {
            let Some(end) = self.marker_us(marker) else {
                return rendered;
            };
            Self::write_record(&mut rendered, phase, turn_ordinal, start, end);
            start = end;
        }
        Self::write_record(&mut rendered, "exit", turn_ordinal, start, exit_us);
        rendered
    }

    fn write_record(
        output: &mut String,
        phase: &'static str,
        turn_ordinal: u64,
        start_us: u64,
        end_us: u64,
    ) {
        let operation_micros = end_us.saturating_sub(start_us);
        let _ = writeln!(
            output,
            "haider: trace level=TRACE target=haider.turn side=client clock=client_exec \
             phase={phase} operation_micros={operation_micros} \
             turn_ordinal={turn_ordinal} request_ordinal=0 txn_ordinal=0 \
             start_us_from_exec={start_us} end_us_from_exec={end_us}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn client_envelope_trace_off_reads_no_clock_and_allocates_no_context() {
        let clock_read = Cell::new(false);
        let trace = ClientEnvelopeTrace::new_if_enabled_with_clock(false, || {
            clock_read.set(true);
            Instant::now()
        });
        assert!(trace.is_none());
        assert!(!clock_read.get());
    }

    #[test]
    fn client_envelope_phases_are_monotonic_and_record_once() {
        let Some(trace) = ClientEnvelopeTrace::new_if_enabled_with_clock(true, Instant::now) else {
            panic!("trace enabled")
        };
        trace.bind_turn_ordinal(41);
        trace.mark_at_us(PROFILE_RESOLVED, 3);
        trace.mark_at_us(PROFILE_RESOLVED, 99);
        trace.mark_at_us(CONNECTED, 7);
        trace.mark_at_us(HELLO_DONE, 11);
        trace.mark_at_us(SUBMITTED, 19);
        trace.mark_at_us(TERMINAL_SEEN, 31);
        trace.mark_at_us(TERMINAL_SEEN, 35);
        let rendered = trace.render_at_us(37);
        for phase in [
            "exec_start",
            "profile_resolved",
            "connected",
            "hello_done",
            "submitted",
            "terminal_seen",
            "exit",
        ] {
            assert_eq!(rendered.matches(&format!("phase={phase} ")).count(), 1);
        }
        assert!(rendered.contains("phase=profile_resolved operation_micros=3"));
        assert!(rendered.contains("phase=exit operation_micros=6"));
        assert!(trace.render_at_us(40).is_empty());
    }
}
