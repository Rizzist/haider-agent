#![allow(clippy::expect_used)]

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

#[cfg(all(unix, not(target_os = "espidf")))]
#[test]
#[allow(unsafe_code)]
fn spawned_daemon_readiness_descriptor_survives_the_bounded_sweep() {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;

    const CHILD_MARKER: &str = "HAIDER_READINESS_SWEEP_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        // SAFETY: descriptor 3 is only borrowed for an open-descriptor probe.
        let readiness = unsafe { std::os::fd::BorrowedFd::borrow_raw(3) };
        rustix::io::fcntl_getfd(readiness)
            .expect("readiness descriptor must survive the inherited-fd sweep");
        return;
    }

    let source = std::fs::File::open("/dev/null").expect("open readiness source");
    let source_fd = source.as_raw_fd();
    let executable = std::env::current_exe().expect("locate platform test binary");
    let output = unsafe {
        std::process::Command::new(executable)
            .arg("spawned_daemon_readiness_descriptor_survives_the_bounded_sweep")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .pre_exec(move || super::install_daemon_readiness_descriptor(source_fd))
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
