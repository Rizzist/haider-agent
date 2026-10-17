//! Darwin sweep permission errors require a positive cleanup witness.
#![allow(clippy::expect_used)]

use super::*;
use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Stdio};

struct PinnedGroup {
    child: Child,
    group: ProcessGroup,
    pid: Pid,
}

impl PinnedGroup {
    fn new(descendant: bool) -> Self {
        let mut command = std::process::Command::new("/usr/bin/perl");
        command.args([
            "-e",
            if descendant {
                "$pid=fork; die $! unless defined $pid; if ($pid) { exit 0 } \
                 print q(r); close STDOUT; alarm 60; while (<STDIN>) {}"
            } else {
                "print q(r); close STDOUT; alarm 60; while (<STDIN>) {}"
            },
        ]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("spawn owned sweep fixture");
        let pid = haider_platform::process_id(Some(child.id())).expect("fixture pid");
        let group = haider_platform::register_process_group(child.id()).expect("fixture group");
        let mut ready = [0];
        child
            .stdout
            .take()
            .expect("fixture stdout")
            .read_exact(&mut ready)
            .expect("fixture readiness after fork");
        assert_eq!(ready, *b"r");
        Self { child, group, pid }
    }

    fn wait_for(&self, mut predicate: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + haider_platform::TEST_WAIT_TIMEOUT;
        while !predicate() {
            assert!(
                std::time::Instant::now() < deadline,
                "fixture did not settle"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_for_zombie(&self) {
        self.wait_for(|| haider_platform::process_leader_exited(self.pid).expect("waitid"));
    }
}

impl Drop for PinnedGroup {
    fn drop(&mut self) {
        self.child.stdin.take();
        let _ = haider_platform::signal_process_group(self.group, Signal::KILL);
        let _ = self.child.wait();
    }
}

fn denied() -> std::io::Result<()> {
    Err(std::io::Error::from_raw_os_error(
        rustix::io::Errno::PERM.raw_os_error(),
    ))
}

/// Mutation: use a single waitid snapshot instead of allowing exit publication.
#[tokio::test(start_paused = true)]
async fn sweep_confirmation_waits_for_publication_and_is_bounded() {
    let started = tokio::time::Instant::now();
    let mut observations = 0;
    assert!(
        confirm_swept_group(|| {
            observations += 1;
            Ok(observations == 3)
        })
        .await
    );
    assert_eq!(observations, 3, "pending exit is observed again");
    assert_eq!(started.elapsed(), Duration::from_millis(2));

    let started = tokio::time::Instant::now();
    assert!(!confirm_swept_group(|| Ok(false)).await);
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    let started = tokio::time::Instant::now();
    assert!(!confirm_swept_group(|| Err(std::io::Error::other("inspection failed"))).await);
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn sweep_permission_recovery_requires_an_exited_leader() {
    let mut fixture = PinnedGroup::new(false);
    for signal in [Signal::TERM, Signal::KILL] {
        let error = finish_sweep_signal(denied(), fixture.group, fixture.pid, signal, false)
            .await
            .expect_err("a live leader must retain its signal failure");
        assert!(error.to_string().contains("Operation not permitted"));
        assert!(
            finish_sweep_signal(Ok(()), fixture.group, fixture.pid, signal, false)
                .await
                .expect("sent signal")
        );
    }
    fixture.child.stdin.take();
    fixture.wait_for_zombie();
    for signal in [Signal::TERM, Signal::KILL] {
        for observed in [false, true] {
            assert!(
                !finish_sweep_signal(denied(), fixture.group, fixture.pid, signal, observed)
                    .await
                    .expect("only the pinned zombie remains")
            );
        }
        assert!(
            !signal_platform_group_for_sweep(fixture.group, fixture.pid, signal, false)
                .await
                .expect("real Darwin zombie-only group is already swept")
        );
    }
    let error = finish_sweep_signal(
        Err(std::io::Error::other("injected signal failure")),
        fixture.group,
        fixture.pid,
        Signal::KILL,
        true,
    )
    .await
    .expect_err("other signal failures are never normalized");
    assert!(error.to_string().contains("injected signal failure"));
    assert!(
        !finish_sweep_signal(
            Err(std::io::Error::from_raw_os_error(
                rustix::io::Errno::SRCH.raw_os_error()
            )),
            fixture.group,
            fixture.pid,
            Signal::KILL,
            false
        )
        .await
        .expect("missing group is already swept")
    );
}

/// Mutation: consider an exited leader sufficient despite a remaining member.
#[tokio::test(start_paused = true)]
async fn sweep_permission_recovery_never_hides_a_live_descendant_denial() {
    let mut fixture = PinnedGroup::new(true);
    fixture.wait_for_zombie();
    assert!(
        !haider_platform::process_group_contains_only_leader(fixture.group, fixture.pid)
            .expect("live descendant")
    );
    for signal in [Signal::TERM, Signal::KILL] {
        for observed in [false, true] {
            let error = finish_sweep_signal(denied(), fixture.group, fixture.pid, signal, observed)
                .await
                .expect_err("leader exit cannot erase a descendant signal denial");
            assert!(error.to_string().contains("Operation not permitted"));
        }
    }
    fixture.child.stdin.take();
    fixture.wait_for(|| {
        haider_platform::process_group_contains_only_leader(fixture.group, fixture.pid)
            .expect("descendant cleanup")
    });
    assert!(
        !finish_sweep_signal(denied(), fixture.group, fixture.pid, Signal::KILL, true)
            .await
            .expect("completed cleanup now has a positive witness")
    );
}
