#![allow(clippy::expect_used)]

#[cfg(unix)]
#[tokio::test]
async fn readiness_byte_buffered_before_wait_is_observed() {
    let prepared = super::prepare_readiness().expect("prepare readiness pipe");
    let super::PreparedReadiness {
        receiver, writer, ..
    } = prepared;

    let written = rustix::io::write(&writer, &[1]).expect("write readiness before awaiting");
    assert_eq!(written, 1);
    drop(writer);

    receiver.wait().await.expect("observe buffered readiness");
}

#[cfg(unix)]
#[tokio::test]
async fn readiness_eof_is_never_treated_as_ready() {
    let prepared = super::prepare_readiness().expect("prepare readiness pipe");
    let super::PreparedReadiness {
        receiver, writer, ..
    } = prepared;
    drop(writer);

    let error = receiver.wait().await.expect_err("EOF must not mean Ready");
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

/// SIGKILL race pin: launcher death may close the writer before the daemon
/// adopts or polls its inherited reader. EOF remains buffered kernel state and
/// must be observed on the first later wait.
#[cfg(unix)]
#[tokio::test]
async fn launcher_eof_before_liveness_wait_is_observed() {
    let prepared = super::prepare_liveness().expect("prepare liveness pipe");
    let super::PreparedLiveness { guard, reader, .. } = prepared;
    drop(guard);

    super::DaemonLivenessWatcher { reader }
        .wait()
        .await
        .expect("observe launcher EOF buffered before wait");
}

#[cfg(unix)]
#[test]
fn readiness_descriptors_are_cloexec_and_writer_is_above_stdio() {
    use std::os::fd::AsRawFd as _;

    let prepared = super::prepare_readiness().expect("prepare readiness pipe");
    let reader_flags = rustix::io::fcntl_getfd(&prepared.receiver.reader).expect("reader flags");
    let writer_flags = rustix::io::fcntl_getfd(&prepared.writer).expect("writer flags");

    assert!(reader_flags.contains(rustix::io::FdFlags::CLOEXEC));
    assert!(writer_flags.contains(rustix::io::FdFlags::CLOEXEC));
    assert!(prepared.writer.as_raw_fd() >= 5);
}

#[cfg(unix)]
#[test]
fn liveness_descriptors_are_cloexec_and_reader_is_above_stdio() {
    use std::os::fd::AsRawFd as _;

    let prepared = super::prepare_liveness().expect("prepare liveness pipe");
    let reader_flags = rustix::io::fcntl_getfd(&prepared.reader).expect("reader flags");
    let writer_flags = rustix::io::fcntl_getfd(&prepared.guard._writer).expect("writer flags");

    assert!(reader_flags.contains(rustix::io::FdFlags::CLOEXEC));
    assert!(writer_flags.contains(rustix::io::FdFlags::CLOEXEC));
    assert!(prepared.reader.as_raw_fd() >= 5);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_readiness_survives_notify_before_wait() {
    const CHILD_MARKER: &str = "HAIDER_READINESS_STDIO_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        super::DaemonReadyNotifier::from_spawn_token(super::DAEMON_READINESS_TOKEN)
            .and_then(super::DaemonReadyNotifier::notify)
            .expect("child publishes readiness through stdin");
        return;
    }

    let prepared = super::prepare_readiness().expect("prepare readiness named pipe");
    let child_stdin = prepared
        .writer
        .try_clone()
        .expect("clone readiness writer for child stdin");
    let receiver = prepared.into_receiver();
    let executable = std::env::current_exe().expect("locate platform test binary");
    let mut child = std::process::Command::new(executable)
        .arg("named_pipe_readiness_survives_notify_before_wait")
        .arg("--nocapture")
        .env(CHILD_MARKER, "1")
        .stdin(std::process::Stdio::from(child_stdin))
        .spawn()
        .expect("spawn readiness notifier child");
    let status = child.wait().expect("wait for readiness notifier child");
    assert!(status.success(), "notifier child failed: {status}");

    receiver.wait().await.expect("observe named-pipe readiness");
}
