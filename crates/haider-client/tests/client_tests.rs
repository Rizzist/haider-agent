//! RpcClient + auto-spawn seam tests against in-process fake daemons.
//!
//! These pin the client-side W3c2 laws: request correlation, the R9
//! heartbeat deadlines on paused time, and the R8 skew rules — a live but
//! old daemon is diagnosed, never killed; no wire overlap is fatal; neither
//! ever spawns a competitor.
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use haider_client::{
    ClientConfig, ConnectError, ConnectionState, DisconnectReason, EnsureError, EnsureOptions,
    ProfileEnv, connect, ensure_daemon, resolve_profile,
};
use haider_rpc::{
    Capability, CapabilitySet, DEFAULT_FRAME_LIMIT, LifecyclePhase, ProtocolError, RequestBody,
    ResponseBody, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const LIMIT: usize = DEFAULT_FRAME_LIMIT;

fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hclt")
        .tempdir_in("/tmp")
        .expect("short temp dir")
}

fn welcome(profile_id: &str, features: BTreeSet<String>) -> Welcome {
    Welcome {
        protocol: WIRE_PROTOCOL_VERSION,
        instance_id: "fake-instance".into(),
        daemon_generation: 1,
        frame_limit: LIMIT as u32,
        profile_id: profile_id.into(),
        daemon_version: "0.0.1-fake".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features,
    }
}

async fn write_frame(stream: &mut UnixStream, frame: &WireFrame) {
    let bytes = uds_codec::encode(frame, LIMIT).expect("encode fake frame");
    stream.write_all(&bytes).await.expect("write fake frame");
}

/// Reads frames until at least one is available.
async fn read_frames(stream: &mut UnixStream, decoder: &mut uds_codec::Decoder) -> Vec<WireFrame> {
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.expect("fake server read");
        if read == 0 {
            return Vec::new();
        }
        let batch = decoder.push(&buffer[..read]);
        assert!(
            batch.error.is_none(),
            "fake server decode: {:?}",
            batch.error
        );
        if !batch.frames.is_empty() {
            return batch.frames;
        }
    }
}

/// A fake daemon that completes the handshake with the given Welcome and
/// then runs `serve` per accepted connection.
fn spawn_fake_daemon<F, Fut>(
    endpoint: &Path,
    accepted: Arc<AtomicUsize>,
    hello_reply: HelloReply,
    serve: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(UnixStream, uds_codec::Decoder) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = UnixListener::bind(endpoint).expect("bind fake daemon");
    tokio::spawn(async move {
        let serve = Arc::new(serve);
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            accepted.fetch_add(1, Ordering::SeqCst);
            let reply = hello_reply.clone();
            let serve = Arc::clone(&serve);
            tokio::spawn(async move {
                let mut decoder = uds_codec::Decoder::new(LIMIT);
                let frames = read_frames(&mut stream, &mut decoder).await;
                assert!(
                    matches!(frames.first(), Some(WireFrame::Hello(_))),
                    "fake daemon expected Hello first"
                );
                match reply {
                    HelloReply::Welcome(welcome) => {
                        write_frame(&mut stream, &WireFrame::Welcome(welcome)).await;
                        serve(stream, decoder).await;
                    }
                    HelloReply::Reject(error) => {
                        write_frame(&mut stream, &WireFrame::ProtocolError(error)).await;
                    }
                }
            });
        }
    })
}

#[derive(Clone)]
enum HelloReply {
    Welcome(Welcome),
    Reject(ProtocolError),
}

/// Echo server body: answers every Ping with a Pong and every Request with a
/// distinguishable error response carrying the request id in its message.
async fn echo_serve(mut stream: UnixStream, mut decoder: uds_codec::Decoder) {
    loop {
        let frames = read_frames(&mut stream, &mut decoder).await;
        if frames.is_empty() {
            return;
        }
        for frame in frames {
            match frame {
                WireFrame::Ping { nonce } => {
                    write_frame(&mut stream, &WireFrame::Pong { nonce }).await;
                }
                WireFrame::Request { request_id, .. } => {
                    write_frame(
                        &mut stream,
                        &WireFrame::Response {
                            request_id: request_id.clone(),
                            body: ResponseBody::Error {
                                code: "echo".into(),
                                message: format!("echo:{}", request_id.as_str()),
                                retryable: false,
                                data: None,
                            },
                        },
                    )
                    .await;
                }
                _ => {}
            }
        }
    }
}

fn list_body() -> RequestBody {
    RequestBody::SessionList {
        cursor: None,
        limit: 1,
    }
}

#[tokio::test]
async fn responses_correlate_by_request_id_even_out_of_order() {
    let dir = short_dir();
    let endpoint = dir.path().join("fake.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let features = haider_client::required_live_features();
    let _daemon = spawn_fake_daemon(
        &endpoint,
        Arc::clone(&accepted),
        HelloReply::Welcome(welcome("profile-x", features)),
        // Out-of-order responder: buffer two requests, answer the SECOND
        // first, then the first.
        |mut stream, mut decoder| async move {
            let mut requests = Vec::new();
            while requests.len() < 2 {
                for frame in read_frames(&mut stream, &mut decoder).await {
                    if let WireFrame::Request { request_id, .. } = frame {
                        requests.push(request_id);
                    }
                }
            }
            for request_id in requests.iter().rev() {
                write_frame(
                    &mut stream,
                    &WireFrame::Response {
                        request_id: request_id.clone(),
                        body: ResponseBody::Error {
                            code: "echo".into(),
                            message: format!("echo:{}", request_id.as_str()),
                            retryable: false,
                            data: None,
                        },
                    },
                )
                .await;
            }
            // Keep the connection open until the client goes away.
            let _ = read_frames(&mut stream, &mut decoder).await;
        },
    );

    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake daemon");
    let client = Arc::new(connected.client);
    let first = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.request(list_body()).await })
    };
    let second = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.request(list_body()).await })
    };
    let first = first.await.expect("join").expect("first response");
    let second = second.await.expect("join").expect("second response");
    // Each response landed on ITS request despite reversed wire order: both
    // messages echo a request id and the two differ.
    let text = |body: &ResponseBody| match body {
        ResponseBody::Error { message, .. } => message.clone(),
        other => panic!("unexpected body {other:?}"),
    };
    let (a, b) = (text(&first), text(&second));
    assert!(a.starts_with("echo:req-") && b.starts_with("echo:req-"));
    assert_ne!(a, b);
}

/// MUTATION CHECK: capture peer credentials after `UnixStream::into_split`
/// or substitute the profile lock's diagnostic PID. Expected RUNTIME
/// failure: the retained kernel PID/UID no longer identify this fake server
/// process exactly.
#[tokio::test]
async fn connect_retains_kernel_authenticated_peer_credentials() {
    let dir = short_dir();
    let probe = dir.path().join("peer-credentials-probe.sock");
    match std::os::unix::net::UnixListener::bind(&probe) {
        Ok(listener) => {
            drop(listener);
            std::fs::remove_file(&probe).expect("remove socket capability probe");
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("bind socket capability probe: {error}"),
    }
    let endpoint = dir.path().join("peer-credentials.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_fake_daemon(
        &endpoint,
        accepted,
        HelloReply::Welcome(welcome(
            "profile-x",
            haider_client::required_live_features(),
        )),
        echo_serve,
    );
    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake daemon");
    assert_eq!(connected.peer_credentials.pid, Some(std::process::id()));
    assert_eq!(
        connected.peer_credentials.uid,
        haider_client::effective_uid()
    );
    connected.client.close();
}

// MUTATION CHECK: the R9 client heartbeat law — a ping unmatched for the
// pong deadline declares the connection dead. Mutating the heartbeat's
// deadline check (`>= pong_deadline` -> `false`, or skipping `fail`) must
// hang this paused-time test's `disconnected()` await (it is bounded by the
// outer timeout below and fails).
#[tokio::test(start_paused = true)]
async fn silent_server_trips_the_pong_deadline_on_paused_time() {
    let dir = short_dir();
    let endpoint = dir.path().join("fake.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_fake_daemon(
        &endpoint,
        Arc::clone(&accepted),
        HelloReply::Welcome(welcome(
            "profile-x",
            haider_client::required_live_features(),
        )),
        // Swallow everything after the handshake: no pongs, no responses.
        |mut stream, mut decoder| async move {
            loop {
                if read_frames(&mut stream, &mut decoder).await.is_empty() {
                    return;
                }
            }
        },
    );
    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake daemon");
    let started = tokio::time::Instant::now();
    let reason = tokio::time::timeout(Duration::from_secs(120), connected.client.disconnected())
        .await
        .expect("pong deadline must fire within the paused-time bound");
    assert_eq!(reason, DisconnectReason::PongTimeout);
    // The first ping goes out immediately; its 45 s deadline is checked on
    // the 15 s tick cadence, so death lands at exactly 45 virtual seconds.
    assert_eq!(started.elapsed(), Duration::from_secs(45));
}

#[tokio::test(start_paused = true)]
async fn answered_pings_keep_the_connection_alive_on_paused_time() {
    let dir = short_dir();
    let endpoint = dir.path().join("fake.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_fake_daemon(
        &endpoint,
        Arc::clone(&accepted),
        HelloReply::Welcome(welcome(
            "profile-x",
            haider_client::required_live_features(),
        )),
        echo_serve,
    );
    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake daemon");
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(connected.client.state(), ConnectionState::Connected);
    // The connection still serves requests after five virtual minutes of
    // pure heartbeat traffic.
    let body = connected
        .client
        .request(list_body())
        .await
        .expect("request after quiescence");
    assert!(matches!(body, ResponseBody::Error { .. }));
}

// MUTATION CHECK: R8's live-but-old rule — wire overlap with missing W3c
// features is an explicit diagnostic and the incumbent daemon is NEVER
// killed or replaced. Mutating `try_attach` to treat missing features as
// spawnable must fail the `Spawn`-variant assertion (the impossible daemon
// binary would be launched) and the still-serving assertion below.
#[tokio::test]
async fn live_but_old_daemon_is_diagnosed_and_never_killed() {
    let dir = short_dir();
    let store = dir.path().join("store");
    let env = ProfileEnv {
        profile_dir: Some(store),
        home: None,
        model: None,
        xdg_runtime_dir: None,
    };
    let mut profile = resolve_profile(&env).expect("resolve profile");
    // Point the profile's endpoint at our fake old daemon.
    profile.endpoint_path = dir.path().join("old.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_fake_daemon(
        &profile.endpoint_path,
        Arc::clone(&accepted),
        // Old daemon: valid v1 Welcome, NO feature families.
        HelloReply::Welcome(welcome(&profile.profile_id, BTreeSet::new())),
        echo_serve,
    );
    let options = EnsureOptions {
        // If ensure_daemon ever tried to spawn, this impossible binary
        // would surface as EnsureError::Spawn instead of MissingFeatures.
        daemon_binary: Some(PathBuf::from("/nonexistent/haiderd-must-not-run")),
        ..EnsureOptions::default()
    };
    let error = match ensure_daemon(&profile, options).await {
        Err(error) => error,
        Ok(_) => panic!("old daemon must not satisfy required features"),
    };
    match &error {
        EnsureError::MissingFeatures { missing, .. } => {
            assert!(missing.contains("session_mutation_v1"));
            assert!(missing.contains("turn_control_v1"));
        }
        other => panic!("expected MissingFeatures, got {other:?}"),
    }
    assert!(
        error
            .to_string()
            .contains("stop/upgrade the running daemon"),
        "skew diagnostic must instruct stop/upgrade: {error}"
    );
    // The incumbent keeps serving: it accepts a fresh connection afterward.
    let again = connect(&profile.endpoint_path, ClientConfig::default())
        .await
        .expect("old daemon must still be serving");
    assert_eq!(again.welcome.profile_id, profile.profile_id);
    assert!(accepted.load(Ordering::SeqCst) >= 2);
}

// MUTATION CHECK (R8 version skew): treat `protocol_version_mismatch` as a
// spawnable failure in `try_attach`. Expected failure: ensure_daemon tries
// the impossible daemon binary and this test sees EnsureError::Spawn
// instead of ProtocolMismatch.
#[tokio::test]
async fn no_wire_overlap_is_a_fatal_mismatch_and_never_spawns() {
    let dir = short_dir();
    let store = dir.path().join("store");
    let env = ProfileEnv {
        profile_dir: Some(store),
        home: None,
        model: None,
        xdg_runtime_dir: None,
    };
    let mut profile = resolve_profile(&env).expect("resolve profile");
    profile.endpoint_path = dir.path().join("mismatch.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_fake_daemon(
        &profile.endpoint_path,
        Arc::clone(&accepted),
        HelloReply::Reject(ProtocolError {
            code: "protocol_version_mismatch".into(),
            message: "client range 1..=1 does not overlap server range 9..=9".into(),
            fatal: true,
        }),
        echo_serve,
    );
    let options = EnsureOptions {
        daemon_binary: Some(PathBuf::from("/nonexistent/haiderd-must-not-run")),
        ..EnsureOptions::default()
    };
    match ensure_daemon(&profile, options).await {
        Err(EnsureError::ProtocolMismatch(error)) => {
            assert_eq!(error.code, "protocol_version_mismatch");
        }
        Err(other) => panic!("expected ProtocolMismatch, got {other:?}"),
        Ok(_) => panic!("expected ProtocolMismatch, got a ready daemon"),
    }
    // Still serving; no competitor was spawned against it.
    let error = match connect(&profile.endpoint_path, ClientConfig::default()).await {
        Err(error) => error,
        Ok(_) => panic!("mismatching daemon must keep rejecting"),
    };
    assert!(matches!(error, ConnectError::Rejected(_)));
}

// MUTATION CHECK (R8 step 4): skip the Welcome.profile_id comparison in
// `try_attach`. Expected failure: the foreign-profile daemon below is
// attached instead of rejected with ProfileMismatch.
#[tokio::test]
async fn profile_mismatch_is_fatal() {
    let dir = short_dir();
    let store = dir.path().join("store");
    let env = ProfileEnv {
        profile_dir: Some(store),
        home: None,
        model: None,
        xdg_runtime_dir: None,
    };
    let mut profile = resolve_profile(&env).expect("resolve profile");
    profile.endpoint_path = dir.path().join("other.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_fake_daemon(
        &profile.endpoint_path,
        Arc::clone(&accepted),
        HelloReply::Welcome(welcome(
            "another-profile-entirely",
            haider_client::required_live_features(),
        )),
        echo_serve,
    );
    let options = EnsureOptions {
        daemon_binary: Some(PathBuf::from("/nonexistent/haiderd-must-not-run")),
        ..EnsureOptions::default()
    };
    match ensure_daemon(&profile, options).await {
        Err(EnsureError::ProfileMismatch { expected, actual }) => {
            assert_eq!(expected, profile.profile_id);
            assert_eq!(actual, "another-profile-entirely");
        }
        Err(other) => panic!("expected ProfileMismatch, got {other:?}"),
        Ok(_) => panic!("expected ProfileMismatch, got a ready daemon"),
    }
}

// MUTATION CHECK (client failure propagation): stop `route_frame` from
// failing the connection on a fatal ProtocolError. Expected failure: the
// pending request below hangs instead of resolving to the typed
// Fatal disconnect (bounded by the suite timeout).
#[tokio::test]
async fn fatal_protocol_error_frame_fails_pending_requests() {
    let dir = short_dir();
    let endpoint = dir.path().join("fatal.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_fake_daemon(
        &endpoint,
        Arc::clone(&accepted),
        HelloReply::Welcome(welcome(
            "profile-x",
            haider_client::required_live_features(),
        )),
        |mut stream, mut decoder| async move {
            // On the first request, answer with a fatal protocol error.
            loop {
                let frames = read_frames(&mut stream, &mut decoder).await;
                if frames.is_empty() {
                    return;
                }
                if frames
                    .iter()
                    .any(|frame| matches!(frame, WireFrame::Request { .. }))
                {
                    write_frame(
                        &mut stream,
                        &WireFrame::ProtocolError(ProtocolError {
                            code: "overloaded".into(),
                            message: "fatal fake".into(),
                            fatal: true,
                        }),
                    )
                    .await;
                    return;
                }
            }
        },
    );
    // Starvation-proofed (W5f-3): under full-gate CPU contention the
    // client's own keepalive deadline could beat the fake daemon's fatal
    // frame, resolving the request to a DIFFERENT disconnect variant and
    // flaking this fixture. The property under test is frame routing, not
    // keepalive timing — give the keepalive room starvation cannot beat.
    let config = ClientConfig {
        ping_interval: Duration::from_secs(120),
        pong_deadline: Duration::from_secs(120),
        ..ClientConfig::default()
    };
    let connected = connect(&endpoint, config)
        .await
        .expect("connect fake daemon");
    let error = connected
        .client
        .request(list_body())
        .await
        .expect_err("fatal error must fail the pending request");
    match error {
        haider_client::ClientError::Disconnected(DisconnectReason::Fatal(protocol_error)) => {
            assert_eq!(protocol_error.code, "overloaded");
        }
        other => panic!("expected fatal disconnect, got {other:?}"),
    }
}

/// W3c2 review finding 1: a request racing `Shared::fail` must never orphan
/// its pending sender. The fix places the disconnect check INSIDE the
/// pending lock, so either the flip is visible before insert (typed error,
/// nothing inserted) or the sender lands before fail's one-time clear and the
/// clear drops it (the receiver resolves with the typed disconnect). The
/// race window itself is not black-box constructible, so this is the
/// r2-precedented executing source guard on the ordering, beside the
/// behavioral request-after-disconnect pin below.
///
/// The correlation now lives in `begin_request` (W3c3 P1-3 split the ordered
/// send from the concurrent wait); `request` is its one-line composition, so
/// there is still exactly ONE copy of this ordering to guard.
///
/// MUTATION CHECK: move the `ConnectionState::Disconnected` early-return in
/// `RpcClient::begin_request` back above the `pending.lock()` acquisition.
/// Expected failure: the position assertions below invert.
#[test]
fn request_disconnect_check_sits_inside_the_pending_lock() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/client.rs"),
    )
    .expect("client source");
    let body_start = source
        .find("pub async fn begin_request")
        .expect("begin_request fn");
    let body = &source[body_start..body_start + 2_500];
    let lock = body
        .find("self.shared.pending.lock()")
        .expect("pending lock acquisition inside request");
    let check = body
        .find("ConnectionState::Disconnected(reason) = self.state()")
        .expect("disconnect check inside request");
    let insert = body.find("pending.insert(").expect("pending insert");
    assert!(
        lock < check,
        "the disconnect check must run under the pending lock (fail flips \
         state before its one-time clear takes the same lock)"
    );
    assert!(
        check < insert,
        "the check precedes the insert so nothing is inserted after the flip"
    );
}

/// Behavioral half of the pin: once the client observes a disconnect, a new
/// `request()` resolves with the typed reason — it must never hang.
#[tokio::test]
async fn request_after_observed_disconnect_resolves_with_the_typed_reason() {
    let dir = short_dir();
    let endpoint = dir.path().join("post-disconnect.sock");
    let accepted = Arc::new(AtomicUsize::new(0));
    let _daemon = spawn_fake_daemon(
        &endpoint,
        Arc::clone(&accepted),
        HelloReply::Welcome(welcome(
            "profile-x",
            haider_client::required_live_features(),
        )),
        |stream, _decoder| async move {
            // Close immediately after the handshake: the client's reader
            // observes EOF and fails the connection.
            drop(stream);
        },
    );
    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake daemon");
    let reason = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        connected.client.disconnected(),
    )
    .await
    .expect("disconnect observed");
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        connected.client.request(list_body()),
    )
    .await
    .expect("request resolves instead of hanging");
    match outcome {
        Err(haider_client::ClientError::Disconnected(observed)) => {
            assert_eq!(observed, reason, "the first disconnect reason wins");
        }
        Err(other) => panic!("expected the typed disconnect, got {other:?}"),
        Ok(body) => panic!("expected the typed disconnect, got Ok({body:?})"),
    }
}
