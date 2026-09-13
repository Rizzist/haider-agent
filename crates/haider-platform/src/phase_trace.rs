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
    StoreAccess,
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
            Self::StoreAccess => "store_access",
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
pub async fn measure<T>(phase: Phase, future: impl Future<Output = T>) -> T {
    #[cfg(unix)]
    {
        if native::enabled() {
            return native::measure(phase, future, true).await;
        }
    }
    #[cfg(not(unix))]
    let _ = phase;
    future.await
}

/// Attribute active control/handler work without calling its idle time RPC or
/// request assembly. Nested specific scopes retain their own exclusive CPU.
pub async fn measure_active<T>(phase: Phase, future: impl Future<Output = T>) -> T {
    #[cfg(unix)]
    {
        if native::enabled() {
            return native::measure(phase, future, false).await;
        }
    }
    #[cfg(not(unix))]
    let _ = phase;
    future.await
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
    use std::future::{Future, poll_fn};
    use std::io::{BufWriter, Write};
    use std::marker::PhantomData;
    use std::path::PathBuf;
    use std::pin::pin;
    use std::rc::Rc;
    use std::sync::{Mutex, OnceLock};

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
        state: Option<(Phase, u64, PollCpu)>,
    }

    impl Scope {
        pub(super) fn new(phase: Phase) -> Self {
            Self {
                state: enabled().then(|| (phase, clock_ns(ClockId::Monotonic), PollCpu::new())),
            }
        }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            if let Some((phase, start, cpu)) = self.state.take() {
                let cpu = cpu.finish();
                record(Row {
                    phase,
                    start,
                    end: clock_ns(ClockId::Monotonic),
                    cpu,
                    waiting: false,
                });
            }
        }
    }

    pub(super) async fn measure<T>(
        phase: Phase,
        future: impl Future<Output = T>,
        record_waits: bool,
    ) -> T {
        let mut future = pin!(future);
        let mut waiting_since = None;
        poll_fn(|cx| {
            if let Some(start) = waiting_since.take() {
                record(Row {
                    phase,
                    start,
                    end: clock_ns(ClockId::Monotonic),
                    cpu: 0,
                    waiting: true,
                });
            }
            let scope = Scope::new(phase);
            let result = future.as_mut().poll(cx);
            drop(scope);
            if record_waits && result.is_pending() {
                waiting_since = Some(clock_ns(ClockId::Monotonic));
            }
            result
        })
        .await
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
                "{{\"schema\":1,\"pid\":{},\"dropped\":{},\"records\":{},\"clock\":\"CLOCK_MONOTONIC\",\"cpu_clock\":\"CLOCK_THREAD_CPUTIME_ID\"}}",
                std::process::id(),
                records.dropped,
                records.rows.len()
            )?;
            for row in &records.rows {
                writeln!(
                    output,
                    "{{\"phase\":\"{}\",\"start_ns\":{},\"end_ns\":{},\"cpu_ns\":{},\"waiting\":{}}}",
                    row.phase.name(),
                    row.start,
                    row.end,
                    row.cpu,
                    row.waiting
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
                assert_eq!(
                    std::fs::read_dir(directory.path())
                        .unwrap_or_else(|error| panic!("{error}"))
                        .count(),
                    1
                );
                return;
            }
            let mut polled = false;
            let mut future = pin!(super::measure(
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
