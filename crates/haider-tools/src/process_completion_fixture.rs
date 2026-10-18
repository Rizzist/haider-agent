//! A descendant answers a FIFO probe only after its supervisor has returned.
#![allow(clippy::expect_used)]

use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) struct NaturalExitFixture {
    workspace: tempfile::TempDir,
    reader: std::fs::File,
    finished: bool,
}

impl NaturalExitFixture {
    pub(crate) fn new() -> Self {
        let workspace = tempfile::tempdir().expect("natural exit workspace");
        let lifetime = workspace.path().join("lifetime.fifo");
        assert!(
            std::process::Command::new("/usr/bin/mkfifo")
                .env_clear()
                .arg(&lifetime)
                .status()
                .expect("create descendant lifetime FIFO")
                .success()
        );
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).expect("nonblocking flag"),
            )
            .open(lifetime)
            .expect("open descendant lifetime reader");
        Self {
            workspace,
            reader,
            finished: false,
        }
    }

    pub(crate) fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    pub(crate) fn command(&self) -> &'static str {
        concat!(
            "/usr/bin/perl -e '$pid=fork; die $! unless defined $pid; ",
            "if ($pid) { alarm 60; while (!-e q(started)) { select undef, undef, undef, 0.01 }; ",
            "print q(leader); exit 0 } ",
            "$SIG{TERM}=q(IGNORE); alarm 60; ",
            "open $life, q(>), q(lifetime.fifo) or die $!; ",
            "syswrite($life, q(r)) == 1 or die $!; ",
            "open $started, q(>), q(started) or die $!; close $started; ",
            "while (!-e q(cleanup)) { ",
            "if (!$answered && -e q(probe)) { syswrite($life, q(a)) == 1 or die $!; $answered=1 }; ",
            "select undef, undef, undef, 0.01 }'",
        )
    }

    pub(crate) async fn assert_alive(&mut self) {
        let mut byte = [0];
        self.reader
            .read_exact(&mut byte)
            .expect("descendant readiness byte");
        assert_eq!(byte, *b"r");
        std::fs::write(self.workspace().join("probe"), b"probe")
            .expect("request post-completion liveness response");
        tokio::time::timeout(haider_platform::TEST_WAIT_TIMEOUT, async {
            loop {
                match self.reader.read(&mut byte) {
                    Ok(1) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    other => panic!("naturally detached descendant did not answer: {other:?}"),
                }
            }
        })
        .await
        .expect("naturally detached descendant answers after supervisor completion");
        assert_eq!(byte, *b"a");
        let alive = self.reader.read(&mut byte);
        assert!(
            matches!(alive, Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "naturally detached descendant closed its lifetime writer: {alive:?}"
        );
    }

    pub(crate) async fn finish(&mut self) {
        std::fs::write(self.workspace().join("cleanup"), b"released")
            .expect("release descendant cleanup");
        tokio::time::timeout(haider_platform::TEST_WAIT_TIMEOUT, async {
            let mut bytes = [0; 8];
            loop {
                match self.reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    other => panic!("unexpected descendant cleanup read: {other:?}"),
                }
            }
        })
        .await
        .expect("descendant closes its lifetime writer after cleanup");
        self.finished = true;
    }
}

impl Drop for NaturalExitFixture {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Release through the fixture protocol on assertion failure too. Do
        // not reconstruct numeric process-group authority after leader reap.
        let _ = std::fs::write(self.workspace().join("cleanup"), b"released");
        let deadline = Instant::now() + haider_platform::TEST_WAIT_TIMEOUT;
        let mut bytes = [0; 8];
        while Instant::now() < deadline {
            match self.reader.read(&mut bytes) {
                Ok(0) => break,
                Err(error) if error.kind() != std::io::ErrorKind::WouldBlock => break,
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}
