#![allow(clippy::expect_used)]

#[cfg(windows)]
#[test]
fn process_exists_reports_current_process_and_rejects_zero() {
    assert!(super::process_exists(std::process::id()));
    assert!(!super::process_exists(0));
}

#[cfg(windows)]
#[test]
fn process_exists_rejects_exited_child_with_retained_handle() {
    let mut child = std::process::Command::new(super::windows_command_interpreter())
        .args(["/d", "/c", "exit /b 0"])
        .spawn()
        .expect("spawn immediate-exit child");
    let pid = child.id();
    assert!(child.wait().expect("reap child").success());
    // `Child` still owns its Windows process handle: existence of the kernel
    // object must not be confused with a process that remains running.
    assert!(!super::process_exists(pid));
    drop(child);
}

#[cfg(windows)]
#[test]
fn process_exists_mapping_rejects_missing_and_retains_query_errors() {
    assert!(!super::windows_process_result_may_be_alive(Ok(None)));
    assert!(!super::windows_process_result_may_be_alive(Ok(Some(
        super::WindowsProcessState {
            alive: false,
            exit_code: Some(0),
            in_any_job: false,
        },
    ))));
    assert!(super::windows_process_result_may_be_alive(Ok(Some(
        super::WindowsProcessState {
            alive: true,
            exit_code: None,
            in_any_job: false,
        },
    ))));
    assert!(!super::windows_process_result_may_be_alive(Err(
        std::io::Error::from_raw_os_error(1168),
    )));
    assert!(super::windows_process_result_may_be_alive(Err(
        std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    )));
}

#[cfg(all(unix, not(target_os = "espidf")))]
#[allow(unsafe_code)]
fn assert_spawn_closes_inherited_descriptors(test_name: &str, descriptor_count: usize) {
    use std::os::fd::{AsRawFd as _, BorrowedFd};

    const CHILD_DESCRIPTORS: &str = "HAIDER_FD_SWEEP_CHILD_DESCRIPTORS";
    if let Some(descriptors) = std::env::var_os(CHILD_DESCRIPTORS) {
        for descriptor in descriptors.to_string_lossy().split(',') {
            let raw = descriptor
                .parse::<std::os::raw::c_int>()
                .expect("child descriptor");
            // SAFETY: the child only borrows each inherited integer for an
            // EBADF probe and never constructs an owner which could close it.
            let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
            let error = rustix::io::fcntl_getfd(borrowed)
                .expect_err("background-process pre-exec must close inherited descriptor");
            assert_eq!(error, rustix::io::Errno::BADF);
        }
        return;
    }

    let source = std::fs::File::open("/dev/null").expect("open descriptor source");
    let mut planted = Vec::with_capacity(descriptor_count);
    for offset in 0..descriptor_count {
        let minimum = std::os::raw::c_int::try_from(32 + offset).expect("fixture descriptor range");
        let descriptor =
            rustix::io::fcntl_dupfd_cloexec(&source, minimum).expect("plant inherited descriptor");
        rustix::io::fcntl_setfd(&descriptor, rustix::io::FdFlags::empty())
            .expect("clear close-on-exec for inheritance fixture");
        planted.push(descriptor);
    }
    let descriptor_list = planted
        .iter()
        .map(|descriptor| descriptor.as_raw_fd().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let executable = std::env::current_exe().expect("locate platform test binary");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build descriptor fixture runtime");
    let output = runtime.block_on(async {
        let mut command = tokio::process::Command::new(executable);
        command
            .arg(test_name)
            .arg("--nocapture")
            .env(CHILD_DESCRIPTORS, descriptor_list);
        super::configure_background_process(&mut command);
        command
            .output()
            .await
            .expect("spawn descriptor-sweep child")
    });
    assert!(
        output.status.success(),
        "descriptor-sweep child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(all(unix, not(target_os = "espidf")))]
#[test]
fn spawned_child_closes_a_known_inherited_descriptor_above_stdio() {
    assert_spawn_closes_inherited_descriptors(
        "spawned_child_closes_a_known_inherited_descriptor_above_stdio",
        1,
    );
}

#[cfg(all(unix, not(target_os = "espidf")))]
#[test]
fn spawned_child_leaks_none_of_twenty_parent_descriptors() {
    assert_spawn_closes_inherited_descriptors(
        "spawned_child_leaks_none_of_twenty_parent_descriptors",
        20,
    );
}

/// MUTATION CHECK: close inherited descriptors in pre-exec instead of marking
/// them CLOEXEC. That consumes std::process's private exec-error pipe, so this
/// nonexistent executable incorrectly returns `Ok(Child)` instead of ENOENT.
#[cfg(all(unix, not(target_os = "espidf")))]
#[test]
fn background_descriptor_sweep_preserves_synchronous_exec_errors() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build exec-error fixture runtime");
    let error = runtime.block_on(async {
        let mut command =
            tokio::process::Command::new("/haider-fixture-this-executable-must-not-exist-wave-964");
        super::configure_background_process(&mut command);
        command
            .spawn()
            .expect_err("missing executable must fail spawn")
    });
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[cfg(all(unix, not(target_os = "espidf")))]
#[test]
#[allow(unsafe_code)]
fn spawned_daemon_startup_descriptors_survive_the_bounded_sweep() {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;

    const CHILD_MARKER: &str = "HAIDER_READINESS_SWEEP_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        for (fd, name) in [(3, "readiness"), (4, "liveness")] {
            // SAFETY: each fixed descriptor is only borrowed for an
            // open-descriptor probe in the spawned child.
            let descriptor = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            rustix::io::fcntl_getfd(descriptor).unwrap_or_else(|_| {
                panic!("{name} descriptor must survive the inherited-fd sweep")
            });
        }
        return;
    }

    let readiness = std::fs::File::open("/dev/null").expect("open readiness source");
    let liveness = std::fs::File::open("/dev/null").expect("open liveness source");
    let readiness_fd = readiness.as_raw_fd();
    let liveness_fd = liveness.as_raw_fd();
    let upper_bound = super::inherited_descriptor_upper_bound();
    let executable = std::env::current_exe().expect("locate platform test binary");
    // SAFETY: std marks pre_exec unsafe because post-fork closures must avoid
    // runtime state; this fixture calls only the audited descriptor installer.
    let output = unsafe {
        std::process::Command::new(executable)
            .arg("spawned_daemon_startup_descriptors_survive_the_bounded_sweep")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .pre_exec(move || {
                super::install_daemon_spawn_descriptors(
                    Some(readiness_fd),
                    Some(liveness_fd),
                    upper_bound,
                )
            })
            .output()
    }
    .expect("spawn readiness-sweep child");
    assert!(
        output.status.success(),
        "readiness-sweep child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

/// MUTATION CHECK: replace either armed kqueue wait with a 50 ms Tokio
/// polling backoff. The observer must deliver the exit without advancing the
/// paused clock, so that mutation cannot reach even its first poll.
#[cfg(target_os = "macos")]
#[tokio::test(start_paused = true)]
async fn armed_kqueue_observes_a_short_command_without_coarse_backoff() {
    use std::future::{Future as _, poll_fn};
    use std::task::Poll;

    // EOF releases a single short-lived child only after both observers are
    // armed. Unlike `sleep 0.005`, this cannot exit during fixture setup on a
    // loaded runner, or let an already-exited probe bypass the kqueue path.
    let mut child = tokio::process::Command::new("/bin/cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn short kqueue fixture");
    let gate = child.stdin.take().expect("retain child exit gate");
    let pid = super::process_id(child.id()).expect("short fixture pid");
    let retained = super::ProcessExitMonitor::capture(pid)
        .expect("arm retained process identity while child is live");

    // Registry #94: watchdog = 30s notification-repair interval / 2 = 15s.
    // It bounds OS scheduling and event delivery strictly before the first
    // kernel repair poll; it is not a command-latency claim. Start it before
    // polling either future, so a late waiter thread cannot earn extra time.
    let watchdog = super::NOTIFICATION_REPAIR_INTERVAL / 2;
    let wall_started = std::time::Instant::now();
    let virtual_started = tokio::time::Instant::now();
    let mut observer = std::pin::pin!(super::observe_process_leader_exit(pid));
    let mut retained = std::pin::pin!(retained.wait());
    poll_fn(|cx| {
        assert!(observer.as_mut().poll(cx).is_pending(), "leader is gated");
        assert!(retained.as_mut().poll(cx).is_pending(), "peer is gated");
        Poll::Ready(())
    })
    .await;
    drop(gate);

    let mut observed = None;
    let mut retained_observed = None;
    let delivered = poll_fn(|cx| {
        if wall_started.elapsed() >= watchdog {
            return Poll::Ready(false);
        }
        if observed.is_none()
            && let Poll::Ready(result) = observer.as_mut().poll(cx)
        {
            observed = Some(result);
        }
        if retained_observed.is_none()
            && let Poll::Ready(result) = retained.as_mut().poll(cx)
        {
            retained_observed = Some(result);
        }
        if observed.is_some() && retained_observed.is_some() {
            return Poll::Ready(true);
        }
        // A runnable task prevents Tokio's paused clock from auto-advancing.
        // Real kernel notifications still wake the two oneshot receivers;
        // any timer-based fallback remains pending, including the 50ms mutant.
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
    let virtual_elapsed = virtual_started.elapsed();
    let wall_elapsed = wall_started.elapsed();
    // A deschedule inside the poll closure must not let a later kernel repair
    // satisfy the event-delivery proof after the watchdog was last checked.
    let delivered = delivered && wall_elapsed < watchdog;
    if !delivered {
        child.start_kill().expect("kill stalled kqueue fixture");
    }
    let status = child.wait().await.expect("reap short kqueue fixture");
    assert!(
        delivered,
        "kqueue delivery before repair: leader={observed:?}, retained={retained_observed:?}, \
         child={status}, wall={wall_elapsed:?}, virtual={virtual_elapsed:?}, watchdog={watchdog:?}"
    );
    assert!(status.success(), "EOF child must exit successfully");
    observed
        .expect("leader delivery")
        .expect("leader exit event");
    retained_observed
        .expect("retained delivery")
        .expect("retained exit event");
    assert_eq!(virtual_elapsed, std::time::Duration::ZERO);
}
