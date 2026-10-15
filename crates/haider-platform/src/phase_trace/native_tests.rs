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
        let trace = std::fs::read_to_string(&paths[0]).unwrap_or_else(|error| panic!("{error}"));
        assert!(trace.contains("\"schema\":4"));
        assert!(trace.contains(
            "\"counters\":{\"bytes_read\":17,\"blocks_hashed\":1,\"reverify_calls\":1,\"rows_read\":0,\"payload_bytes\":0,\"events_decoded\":0}"
        ));
        assert!(trace.lines().any(|line| {
            line.contains("\"phase\":\"store_query_replay\"")
                && line.contains("\"store_caller\":\"prompt_history\"")
        }));
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
    drop(super::super::store_scope_for(
        Phase::StoreQueryReplay,
        StoreReadCaller::PromptHistory,
    ));
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

// Use a fresh process because DIRECTORY is intentionally fixed at first use.
fn run_record_stream(test: &str, count: usize, enabled: bool, workers: usize) {
    const CHILD: &str = "HAIDER_PHASE_RECORD_STREAM_CHILD";
    if std::env::var_os(CHILD).is_some() {
        std::thread::scope(|threads| {
            for worker in 0..workers {
                let records = count / workers + usize::from(worker < count % workers);
                threads.spawn(move || {
                    for _ in 0..records {
                        drop(super::super::scope(Phase::Rpc));
                    }
                });
            }
        });
        let records = RECORDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if enabled {
            assert_eq!(records.rows.len(), count.min(MAX_RECORDS));
            assert_eq!(records.dropped, count.saturating_sub(MAX_RECORDS));
            assert!(records.rows.capacity() <= MAX_RECORDS * 2);
        } else {
            assert!(records.rows.is_empty());
            assert_eq!(records.rows.capacity(), 0);
            assert_eq!(records.dropped, 0);
            CHILD_CPU.with(|stack| assert_eq!(stack.borrow().capacity(), 0));
            assert!(!super::super::cas_hash_scope(false).enabled());
        }
        drop(records);
        flush();
        return;
    }
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let mut command = std::process::Command::new(
        std::env::current_exe().unwrap_or_else(|error| panic!("{error}")),
    );
    command
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, "1");
    if enabled {
        command.env("HAIDER_PHASE_TRACE_DIR", directory.path());
    } else if test.ends_with("empty_directory_disables_scopes") {
        command.env("HAIDER_PHASE_TRACE_DIR", "");
    } else {
        command.env_remove("HAIDER_PHASE_TRACE_DIR");
    }
    let output = command.output().unwrap_or_else(|error| panic!("{error}"));
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let paths = std::fs::read_dir(directory.path())
        .unwrap_or_else(|error| panic!("{error}"))
        .map(|entry| entry.unwrap_or_else(|error| panic!("{error}")).path())
        .collect::<Vec<_>>();
    if enabled {
        assert_eq!(paths.len(), 1);
        let trace = std::fs::read_to_string(&paths[0]).unwrap_or_else(|error| panic!("{error}"));
        let header = trace
            .lines()
            .next()
            .unwrap_or_else(|| panic!("missing trace header"));
        assert!(header.contains(&format!(
            "\"dropped\":{}",
            count.saturating_sub(MAX_RECORDS)
        )));
        assert!(header.contains(&format!("\"records\":{}", count.min(MAX_RECORDS))));
        assert_eq!(trace.lines().count(), count.min(MAX_RECORDS) + 1);
    } else {
        assert!(paths.is_empty());
    }
}

#[test]
fn records_at_cap_are_complete() {
    run_record_stream(
        "phase_trace::native::tests::records_at_cap_are_complete",
        MAX_RECORDS,
        true,
        1,
    );
}

#[test]
fn records_past_cap_are_explicitly_truncated() {
    run_record_stream(
        "phase_trace::native::tests::records_past_cap_are_explicitly_truncated",
        MAX_RECORDS + 1,
        true,
        1,
    );
}

#[test]
fn long_record_streams_remain_bounded() {
    // Both the observed 60-case size and a much longer attribution request.
    run_record_stream(
        "phase_trace::native::tests::long_record_streams_remain_bounded",
        400_000,
        true,
        1,
    );
}

#[test]
fn disabled_scopes_do_not_allocate_or_record() {
    run_record_stream(
        "phase_trace::native::tests::disabled_scopes_do_not_allocate_or_record",
        175_000,
        false,
        1,
    );
}

#[test]
fn empty_directory_disables_scopes() {
    run_record_stream(
        "phase_trace::native::tests::empty_directory_disables_scopes",
        175_000,
        false,
        1,
    );
}

#[test]
fn concurrent_recorders_share_process_cap() {
    run_record_stream(
        "phase_trace::native::tests::concurrent_recorders_share_process_cap",
        320_000,
        true,
        8,
    );
}
