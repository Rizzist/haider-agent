#![allow(clippy::expect_used)]
#![cfg(unix)]

use super::super::discovery::{
    CurlRequestObservation, CurlTransport, ReleaseSource, UPDATE_CHECK_EXIT_BUDGET, UpdateTransport,
};
use super::*;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, mpsc as sync_mpsc};
use std::time::Instant;

/// Real curl remains blocked in a loopback response while the TUI closes.
/// Teardown must kill/reap that exact PID and join its watcher before returning.
#[test]
fn slow_explicit_update_check_cancels_and_reaps_curl_before_runtime_teardown() {
    stalled_request_teardown(true);
}

/// A bounded-response error must also stop/join the watcher and reap curl,
/// even while the TUI remains open and the server keeps the connection alive.
#[test]
fn explicit_update_response_error_reaps_curl_and_joins_watcher() {
    stalled_request_teardown(false);
}

fn stalled_request_teardown(close_tui: bool) {
    let profile = tempfile::tempdir().expect("profile");
    let profile_dir = profile.path().to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let (requested_tx, requested_rx) = sync_mpsc::channel();
    let (release_tx, release_rx) = sync_mpsc::channel();
    let (peer_closed_tx, peer_closed_rx) = sync_mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("curl connects");
        stream
            .set_read_timeout(Some(UPDATE_CHECK_EXIT_BUDGET))
            .expect("fixture read budget");
        let mut request = Vec::new();
        let mut bytes = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut bytes) {
                Ok(0) | Err(_) => return,
                Ok(read) => request.extend_from_slice(&bytes[..read]),
            }
            if request.len() > 8192 {
                return;
            }
        }
        if !close_tui {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n")
                .expect("oversized response prefix");
            // Fill curl's ordinary pipe-output buffer while keeping the
            // declared response unfinished and its connection open.
            // curl can buffer more than 8KiB of stdout; this forces bytes into
            // its real pipe before the intentionally incomplete body ends.
            // A concurrent bound-triggered kill may close the socket mid-write.
            let _ = stream.write_all(&[b'x'; 64 * 1024]);
        }
        let _ = requested_tx.send(());
        let mut peer_byte = [0_u8; 1];
        let _ = peer_closed_tx.send(stream.read(&mut peer_byte));
        // Keep the real socket open until after the teardown observation.
        let _ = release_rx.recv();
        let _ = stream.shutdown(Shutdown::Both);
    });
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (observed_tx, observed_rx) = sync_mpsc::channel();
    let (outcome_tx, outcome_rx) = sync_mpsc::channel();
    let (stopped_tx, stopped_rx) = sync_mpsc::channel();
    let runtime_owner = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let worker = {
            let _entered = runtime.enter();
            spawn_explicit_check(profile_dir, events_tx, move |cancellation| {
                let mut transport = CurlTransport::without_token()
                    .with_cancellation(cancellation)
                    .with_request_observer_for_test(Arc::new(move |observation| {
                        let _ = observed_tx.send(observation);
                    }));
                let result = if close_tui {
                    super::super::check_update_availability_with(
                        &mut transport,
                        &ReleaseSource {
                            api_base: format!("http://{address}"),
                            repository: "owner/repo".into(),
                            allow_http: true,
                        },
                        "1.0.0",
                        "aarch64-apple-darwin",
                    )
                } else {
                    transport
                        .get_bytes(&format!("http://{address}/releases"), 4)
                        .map(|_| UpdateAvailability::Current {
                            version: "1.0.0".into(),
                        })
                };
                let _ = outcome_tx.send(result.as_ref().err().map(ToString::to_string));
                result
            })
        };
        drop(runtime);
        let _ = stopped_tx.send(worker.is_finished());
    });

    // Registry #94: setup wraps two observations (spawn + received headers),
    // each allotted the existing TUI_EXIT 2.5s budget. The actual shutdown
    // observation gets exactly TUI_EXIT, matching the unchanged QA reap law.
    let setup_deadline = Instant::now() + UPDATE_CHECK_EXIT_BUDGET * 2;
    let spawned =
        observed_rx.recv_timeout(setup_deadline.saturating_duration_since(Instant::now()));
    let requested =
        requested_rx.recv_timeout(setup_deadline.saturating_duration_since(Instant::now()));
    let exit_deadline = Instant::now() + UPDATE_CHECK_EXIT_BUDGET;
    let mut receiver = Some(events_rx);
    if close_tui {
        drop(receiver.take());
    }
    let stopped = stopped_rx.recv_timeout(exit_deadline.saturating_duration_since(Instant::now()));
    let peer_closed =
        peer_closed_rx.recv_timeout(exit_deadline.saturating_duration_since(Instant::now()));

    // Always release the fixture before asserting, so a regression still
    // joins its curl/blocking worker instead of leaking test-owned work.
    let _ = release_tx.send(());
    if requested.is_err() {
        let _ = TcpStream::connect(address);
    }
    drop(receiver);
    runtime_owner.join().expect("runtime owner joined");
    server.join().expect("fixture joined");
    assert!(
        requested.is_ok(),
        "real curl must reach the stalled fixture"
    );
    assert!(stopped.expect("runtime exited within TUI_EXIT"));
    match peer_closed.expect("loopback peer closed within TUI_EXIT") {
        Ok(0) => {}
        // Killing curl with unread response bytes can reset TCP instead of
        // sending FIN. Both are an observed close of this actual connection.
        Err(error) if !close_tui && error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("stalled loopback peer did not close: {result:?}"),
    }
    let pid = match spawned.expect("actual curl PID") {
        CurlRequestObservation::Spawned(pid) => pid,
        observation => panic!("first observation must identify curl: {observation:?}"),
    };
    let observations = observed_rx.try_iter().collect::<Vec<_>>();
    match observations.as_slice() {
        [
            CurlRequestObservation::Reaped {
                pid: reaped_pid,
                status,
            },
            CurlRequestObservation::WatcherJoined,
        ] => {
            assert_eq!(*reaped_pid, pid, "the actual spawned curl must be reaped");
            assert_eq!(
                haider_platform::exit_signal(status),
                Some(9),
                "receipt must carry the real kill/wait status"
            );
        }
        observations => panic!(
            "expected exact Spawned -> Reaped -> WatcherJoined ownership chain: {observations:?}"
        ),
    }
    let error = outcome_rx
        .try_recv()
        .expect("checker outcome")
        .expect("check interrupted");
    assert!(
        error.contains(if close_tui {
            "cancelled"
        } else {
            "configured bound"
        }),
        "unexpected check error: {error}"
    );
}

#[test]
fn cancelled_update_discovery_starts_no_curl_request() {
    let (observed_tx, observed_rx) = sync_mpsc::channel();
    let mut transport = CurlTransport::without_token()
        .with_cancellation(Arc::new(|| true))
        .with_request_observer_for_test(Arc::new(move |observation| {
            let _ = observed_tx.send(observation);
        }));
    for _ in 0..2 {
        let error = transport
            .get_bytes("http://127.0.0.1:1/releases", 4)
            .expect_err("closed TUI refuses every request/page");
        assert!(error.to_string().contains("cancelled"));
    }
    assert!(
        observed_rx.try_recv().is_err(),
        "no child may spawn after closure"
    );
}
