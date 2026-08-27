#![allow(clippy::expect_used)]

#[cfg(all(unix, not(target_os = "espidf")))]
#[test]
#[allow(unsafe_code)]
fn inherited_descriptor_sweep_closes_a_high_descriptor() {
    use std::os::fd::{AsRawFd as _, BorrowedFd};

    const CHILD_MARKER: &str = "HAIDER_FD_SWEEP_TEST_CHILD";
    if std::env::var_os(CHILD_MARKER).is_none() {
        let executable = std::env::current_exe().expect("locate platform test binary");
        let output = std::process::Command::new(executable)
            .arg("inherited_descriptor_sweep_closes_a_high_descriptor")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .output()
            .expect("spawn isolated descriptor-sweep test");
        assert!(
            output.status.success(),
            "isolated descriptor-sweep test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let source = std::fs::File::open("/dev/null").expect("open descriptor source");
    let planted = rustix::io::fcntl_dupfd_cloexec(&source, 333).expect("plant high descriptor");
    let raw = planted.as_raw_fd();
    std::mem::forget(planted);

    super::close_inherited_descriptors_from(raw);

    // SAFETY: borrowing a raw descriptor is permitted for the failing EBADF
    // probe; no owned handle is constructed and therefore no second close runs.
    let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
    let error = rustix::io::fcntl_getfd(borrowed).expect_err("sweep must close planted descriptor");
    assert_eq!(error, rustix::io::Errno::BADF);
}

#[cfg(unix)]
#[test]
fn timed_out_child_wait_does_not_pin_runtime_shutdown() {
    let child = std::process::Command::new("sh")
        .args(["-c", "sleep 2"])
        .spawn()
        .expect("spawn wait fixture");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build fixture runtime");
    let started = std::time::Instant::now();
    let timed_out = runtime.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            super::wait_for_child_exit(child),
        )
        .await
        .is_err()
    });
    assert!(timed_out, "fixture child should outlive the wait deadline");
    drop(runtime);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "runtime shutdown waited for the detached child waiter"
    );
}
