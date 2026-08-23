#![cfg(unix)]
//! W3c3 P1-3 — the ordered-send half of a correlated request.
//!
//! `request` is one round trip with two halves that want opposite things:
//! the WAIT must be concurrent (a slow response cannot stall a live event
//! stream) and the SEND must be sequential (the daemon's
//! `max_attachments_per_connection` rejects a `session.attach` that overtakes
//! the `session.detach` freeing its slot). `begin_request` +
//! [`PendingResponse::wait`] is that split, and these tests pin it: the
//! frame is on the wire when `begin_request` RETURNS, and the wait still
//! resolves — with a body, or with the typed disconnect.
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use haider_client::client::PendingResponse;
use haider_client::{ClientConfig, DisconnectReason, connect};
use haider_rpc::{
    AttachmentId, Capability, CapabilitySet, DEFAULT_FRAME_LIMIT, LifecyclePhase, RequestBody,
    ResponseBody, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const LIMIT: usize = DEFAULT_FRAME_LIMIT;
/// A wire round trip that must not outlive a wedged test.
const BOUND: Duration = Duration::from_secs(20);

fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hcos")
        .tempdir_in("/tmp")
        .expect("short temp dir")
}

fn welcome() -> Welcome {
    Welcome {
        protocol: WIRE_PROTOCOL_VERSION,
        instance_id: "fake-instance".into(),
        daemon_generation: 1,
        frame_limit: LIMIT as u32,
        profile_id: "profile-x".into(),
        daemon_version: "0.0.1-fake".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features: BTreeSet::new(),
        user_command_withheld: false,
        encoding: None,
    }
}

async fn write_frame(stream: &mut UnixStream, frame: &WireFrame) {
    let bytes = uds_codec::encode(frame, LIMIT).expect("encode fake frame");
    stream.write_all(&bytes).await.expect("write fake frame");
}

async fn read_frames(stream: &mut UnixStream, decoder: &mut uds_codec::Decoder) -> Vec<WireFrame> {
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.expect("fake server read");
        if read == 0 {
            return Vec::new();
        }
        let batch = decoder.push(&buffer[..read]);
        assert!(batch.error.is_none(), "fake decode: {:?}", batch.error);
        if !batch.frames.is_empty() {
            return batch.frames;
        }
    }
}

/// A fake daemon that answers `Hello` with a `Welcome` and then runs `serve`.
fn spawn_fake_peer<F, Fut>(endpoint: &Path, serve: F) -> tokio::task::JoinHandle<()>
where
    F: FnOnce(UnixStream, uds_codec::Decoder) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = UnixListener::bind(endpoint).expect("bind fake peer");
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut decoder = uds_codec::Decoder::new(LIMIT);
        let frames = read_frames(&mut stream, &mut decoder).await;
        assert!(
            matches!(frames.first(), Some(WireFrame::Hello(_))),
            "fake peer expected Hello first"
        );
        write_frame(&mut stream, &WireFrame::Welcome(welcome())).await;
        serve(stream, decoder).await;
    })
}

/// A request body distinguishable by name on the wire. `session.detach` is
/// used because its only field IS the identity we assert on, and it needs no
/// type from a crate `haider-client` does not depend on.
fn detach_body(name: &str) -> RequestBody {
    RequestBody::SessionDetach {
        attachment_id: AttachmentId::new(name.to_owned()),
    }
}

/// The identity a recorded request body carries, for order assertions.
fn label(body: &RequestBody) -> String {
    match body {
        RequestBody::SessionDetach { attachment_id } => attachment_id.as_str().to_owned(),
        other => format!("unexpected:{other:?}"),
    }
}

/// THE LAW: `begin_request` returns only once its frame is written, so two
/// requests awaited in sequence reach the daemon in that sequence — even
/// though NEITHER has been answered yet.
///
/// MUTATION CHECK: move the `outbound.send(bytes).await` in
/// `RpcClient::begin_request` into the returned `PendingResponse` (i.e. send
/// lazily on `wait`). Expected failure: the peer observes no frames at all
/// while both `begin_request` calls have already returned, and the
/// timeout below fires instead of the order assertion passing.
#[tokio::test]
async fn begin_request_writes_before_it_returns_so_sends_stay_ordered() {
    let dir = short_dir();
    let endpoint = dir.path().join("ordered.sock");
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let _peer = spawn_fake_peer(&endpoint, |mut stream, mut decoder| async move {
        let mut done = Some(done_tx);
        loop {
            let frames = read_frames(&mut stream, &mut decoder).await;
            if frames.is_empty() {
                return;
            }
            for frame in frames {
                if let WireFrame::Request { body, .. } = frame
                    && let Ok(mut seen) = recorder.lock()
                {
                    seen.push(label(&body));
                    if seen.len() == 2
                        && let Some(done) = done.take()
                    {
                        let _ = done.send(());
                    }
                }
            }
        }
    });

    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake peer");
    let client = connected.client;
    // Neither request is answered; both must still be ON THE WIRE, in order,
    // by the time these awaits return.
    let first: PendingResponse = client
        .begin_request(detach_body("first"))
        .await
        .expect("first frame reaches the wire");
    let second: PendingResponse = client
        .begin_request(detach_body("second"))
        .await
        .expect("second frame reaches the wire");
    tokio::time::timeout(BOUND, done_rx)
        .await
        .expect("the peer must observe both frames")
        .expect("recorder alive");
    let order = seen.lock().expect("recorder").clone();
    assert_eq!(
        order,
        vec!["first".to_owned(), "second".to_owned()],
        "frames reach the daemon in the order their sends were awaited"
    );
    drop((first, second));
}

/// The wait half still resolves with the daemon's body, so splitting the
/// call did not cost correlation.
#[tokio::test]
async fn pending_response_wait_resolves_with_the_correlated_body() {
    let dir = short_dir();
    let endpoint = dir.path().join("wait.sock");
    let _peer = spawn_fake_peer(&endpoint, |mut stream, mut decoder| async move {
        loop {
            let frames = read_frames(&mut stream, &mut decoder).await;
            if frames.is_empty() {
                return;
            }
            for frame in frames {
                if let WireFrame::Request { request_id, .. } = frame {
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
            }
        }
    });
    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake peer");
    let pending = connected
        .client
        .begin_request(detach_body("echoed"))
        .await
        .expect("send half");
    let body = tokio::time::timeout(BOUND, pending.wait())
        .await
        .expect("wait resolves")
        .expect("response body");
    match body {
        ResponseBody::Error { message, .. } => assert!(message.starts_with("echo:req-")),
        other => panic!("unexpected body {other:?}"),
    }
}

/// A correlation dropped by a dying connection resolves as the typed
/// disconnect. A `PendingResponse` parked on a task must never become a
/// permanent hang — that is the shape that wedges a whole UI.
#[tokio::test]
async fn pending_response_wait_reports_the_typed_disconnect() {
    let dir = short_dir();
    let endpoint = dir.path().join("dead.sock");
    let _peer = spawn_fake_peer(&endpoint, |mut stream, mut decoder| async move {
        // Take the request, then vanish without answering it.
        let _ = read_frames(&mut stream, &mut decoder).await;
        drop(stream);
    });
    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake peer");
    let pending = connected
        .client
        .begin_request(detach_body("orphan"))
        .await
        .expect("send half");
    let outcome = tokio::time::timeout(BOUND, pending.wait())
        .await
        .expect("wait resolves instead of hanging");
    match outcome {
        // Reader EOF (PeerClosed) and the writer's failed flush (Io) race
        // legitimately under the client's documented first-reason-wins rule;
        // either typed disconnect satisfies the law under test — the pending
        // wait resolves with a TYPED reason instead of hanging (r2 NF-3).
        Err(haider_client::ClientError::Disconnected(
            DisconnectReason::PeerClosed | DisconnectReason::Io(_),
        )) => {}
        Err(other) => panic!("expected a typed disconnect, got {other:?}"),
        Ok(body) => panic!("expected a disconnect, got Ok({body:?})"),
    }
}
