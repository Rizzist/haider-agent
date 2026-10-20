#![allow(clippy::expect_used)]

use super::fake_cancellation_settlement_gate;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn cancellation_settlement_gate_waits_for_fixture_release_or_daemon_drain() {
    for daemon_drain in [false, true] {
        let directory = tempfile::Builder::new()
            .prefix("hcs-")
            .tempdir_in("/tmp")
            .expect("short cancellation gate directory");
        let path = directory.path().join("cancel.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind fixture gate");
        let (drain, mut drain_wakes) = tokio::sync::watch::channel(false);
        let mut worker = tokio::spawn(async move {
            fake_cancellation_settlement_gate(&path, &mut drain_wakes).await
        });
        let (mut fixture, _) =
            tokio::time::timeout(haider_platform::TEST_WAIT_TIMEOUT, listener.accept())
                .await
                .expect("worker connects to gate")
                .expect("accept worker");
        let mut ready = [0];
        tokio::time::timeout(
            haider_platform::TEST_WAIT_TIMEOUT,
            fixture.read_exact(&mut ready),
        )
        .await
        .expect("worker signals committed cancellation")
        .expect("read readiness receipt");
        assert_eq!(ready, [1]);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut worker)
                .await
                .is_err(),
            "a readiness receipt cannot release cancellation settlement"
        );
        if daemon_drain {
            drain.send_replace(true);
        } else {
            fixture.write_all(&[1]).await.expect("release worker");
        }
        tokio::time::timeout(haider_platform::TEST_WAIT_TIMEOUT, worker)
            .await
            .expect("settlement responds to its owning event")
            .expect("worker joins")
            .expect("settlement gate completes");
    }
}
