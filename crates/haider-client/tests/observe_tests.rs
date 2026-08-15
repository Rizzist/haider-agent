#![cfg(unix)]
//! H1 observation stream cursor and forward-compatibility laws.
#![allow(clippy::expect_used)]

use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use haider_client::{
    ObserveClient, ObserveError, ProfileEnv, ResolvedProfile, observe_stream_session,
    resolve_profile,
};
use haider_rpc::haider_protocol::envelope::{
    PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_rpc::haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_rpc::{
    AttachMode, AttachState, AttachmentId, Capability, CapabilitySet, DEFAULT_FRAME_LIMIT,
    ERROR_CODE_NOT_FOUND, FEATURE_SESSION_FLEET_V1, LifecyclePhase, RequestBody, RequestId,
    ResponseBody, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

const LIMIT: usize = DEFAULT_FRAME_LIMIT;
const BOUND: Duration = Duration::from_secs(5);

struct Peer {
    stream: UnixStream,
    decoder: uds_codec::Decoder,
    queued: VecDeque<WireFrame>,
}

impl Peer {
    async fn next(&mut self) -> WireFrame {
        loop {
            if let Some(frame) = self.queued.pop_front() {
                if let WireFrame::Ping { nonce } = frame {
                    self.write(&WireFrame::Pong { nonce }).await;
                    continue;
                }
                return frame;
            }
            let mut bytes = [0_u8; 8192];
            let read = self.stream.read(&mut bytes).await.expect("peer read");
            assert!(read > 0, "client closed before expected frame");
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "peer decode: {:?}", batch.error);
            self.queued.extend(batch.frames);
        }
    }

    async fn request(&mut self) -> (RequestId, RequestBody) {
        match self.next().await {
            WireFrame::Request { request_id, body } => (request_id, body),
            other => panic!("expected request, got {other:?}"),
        }
    }

    async fn write(&mut self, frame: &WireFrame) {
        let bytes = uds_codec::encode(frame, LIMIT).expect("peer encode");
        self.stream.write_all(&bytes).await.expect("peer write");
    }

    async fn write_batch(&mut self, frames: &[WireFrame]) {
        let mut bytes = Vec::new();
        for frame in frames {
            bytes.extend(uds_codec::encode(frame, LIMIT).expect("peer batch encode"));
        }
        self.stream
            .write_all(&bytes)
            .await
            .expect("peer batch write");
    }

    async fn respond(&mut self, request_id: RequestId, body: ResponseBody) {
        self.write(&WireFrame::Response { request_id, body }).await;
    }
}

fn profile() -> (tempfile::TempDir, ResolvedProfile) {
    let root = tempfile::Builder::new()
        .prefix("hobs")
        .tempdir_in("/tmp")
        .expect("short profile root");
    let profile = resolve_profile(&ProfileEnv {
        profile_dir: Some(root.path().join("profile")),
        home: None,
        model: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve profile");
    std::fs::create_dir_all(&profile.runtime_dir).expect("runtime dir");
    (root, profile)
}

fn welcome(profile: &ResolvedProfile, instance: &str) -> Welcome {
    Welcome {
        protocol: WIRE_PROTOCOL_VERSION,
        instance_id: instance.into(),
        daemon_generation: 1,
        frame_limit: LIMIT as u32,
        profile_id: profile.profile_id.clone(),
        daemon_version: "observe-test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View]),
        features: BTreeSet::new(),
    }
}

async fn accept_peer(listener: &UnixListener, welcome: Welcome) -> Peer {
    let (mut stream, _) = listener.accept().await.expect("accept client");
    let mut decoder = uds_codec::Decoder::new(LIMIT);
    let mut bytes = [0_u8; 8192];
    let read = stream.read(&mut bytes).await.expect("read hello");
    let batch = decoder.push(&bytes[..read]);
    assert!(batch.error.is_none());
    let Some(WireFrame::Hello(hello)) = batch.frames.first() else {
        panic!("expected hello");
    };
    assert_eq!(
        hello.capabilities_requested,
        CapabilitySet::from([Capability::View])
    );
    let encoded = uds_codec::encode(&WireFrame::Welcome(welcome), LIMIT).expect("welcome");
    stream.write_all(&encoded).await.expect("write welcome");
    Peer {
        stream,
        decoder,
        queued: batch.frames.into_iter().skip(1).collect(),
    }
}

fn raw(session_id: &SessionId, seq: u64, detail: &str) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("observe-event-{seq}")),
        seq,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("observe-peer"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1_800_000_000_000 + seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({
            "type": "future_observe_kind_v9",
            "detail": detail,
            "additive": {"kept": true}
        }),
    }
}

async fn accept_attach(
    peer: &mut Peer,
    session_id: &SessionId,
    expected_after: u64,
    attachment: &str,
) -> AttachmentId {
    let (request_id, body) = peer.request().await;
    let RequestBody::SessionAttach {
        session_id: attached,
        after_seq,
        mode,
    } = body
    else {
        panic!("expected session.attach");
    };
    assert_eq!(&attached, session_id);
    assert_eq!(after_seq, expected_after);
    assert_eq!(mode, AttachMode::View);
    let attachment_id = AttachmentId::new(attachment);
    peer.respond(
        request_id,
        ResponseBody::SessionAttach {
            attachment_id: attachment_id.clone(),
            attach_state: AttachState {
                session_id: session_id.clone(),
                requested_after_seq: expected_after,
                replay_through_seq: 3,
                worker_generation: 7,
                authority_epoch: 1,
            },
        },
    )
    .await;
    attachment_id
}

async fn send_event(
    peer: &mut Peer,
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    envelope: RawEnvelope,
) {
    peer.write(&WireFrame::Event {
        attachment_id: attachment_id.clone(),
        session_id: session_id.clone(),
        envelope,
    })
    .await;
}

/// The stream-ends law, checked at runtime: `observe_stream_session` owns the
/// only sender, so once its future resolves every admitted envelope is
/// already buffered AND the channel is closed. Draining with `try_recv`
/// therefore needs no deadline at all — and `Empty` before `Disconnected`
/// means a retained sender clone (the silent-hang class) and fails the test
/// immediately instead of wedging a blocking `recv` forever.
fn drain_ended_stream(receiver: &mut mpsc::UnboundedReceiver<RawEnvelope>) -> Vec<RawEnvelope> {
    let mut envelopes = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok(envelope) => envelopes.push(envelope),
            Err(mpsc::error::TryRecvError::Disconnected) => return envelopes,
            Err(mpsc::error::TryRecvError::Empty) => panic!(
                "stream-ends law violated: the stream future resolved but its \
                 output sender is still live"
            ),
        }
    }
}

/// Fleet snapshot failures remain machine-distinct: feature skew is caught
/// before a request and an existing feature's not-found reply retains the
/// requested session id. Hermetic hosts that deny AF_UNIX binds exercise the
/// same enum mapping in the CLI suite and skip only this transport scenario.
#[tokio::test]
async fn fleet_reads_return_typed_feature_and_unknown_session_errors() {
    let (_old_root, old_profile) = profile();
    let old_listener = match UnixListener::bind(&old_profile.endpoint_path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("bind old fleet peer: {error}"),
    };
    let old_server_profile = old_profile.clone();
    let old_server = tokio::spawn(async move {
        let _peer = accept_peer(
            &old_listener,
            welcome(&old_server_profile, "fleet-old-daemon"),
        )
        .await;
    });
    let old_client = ObserveClient::connect(&old_profile, false)
        .await
        .expect("connect old daemon");
    assert!(matches!(
        old_client
            .fleet(SessionId::new("fleet-feature-session"))
            .await,
        Err(ObserveError::MissingFeature(FEATURE_SESSION_FLEET_V1))
    ));
    old_client.close();
    old_server.await.expect("old daemon peer");

    let (_new_root, new_profile) = profile();
    let new_listener = UnixListener::bind(&new_profile.endpoint_path).expect("bind fleet peer");
    let new_server_profile = new_profile.clone();
    let new_server = tokio::spawn(async move {
        let mut advertised = welcome(&new_server_profile, "fleet-new-daemon");
        advertised
            .features
            .insert(FEATURE_SESSION_FLEET_V1.to_owned());
        let mut peer = accept_peer(&new_listener, advertised).await;
        let (request_id, body) = peer.request().await;
        assert!(matches!(
            body,
            RequestBody::SessionFleet { ref session_id }
                if session_id.as_str() == "fleet-missing-session"
        ));
        peer.respond(
            request_id,
            ResponseBody::Error {
                code: ERROR_CODE_NOT_FOUND.into(),
                message: "session was not found".into(),
                retryable: false,
                data: None,
            },
        )
        .await;
    });
    let new_client = ObserveClient::connect(&new_profile, false)
        .await
        .expect("connect fleet daemon");
    assert!(matches!(
        new_client
            .fleet(SessionId::new("fleet-missing-session"))
            .await,
        Err(ObserveError::UnknownSession(ref session_id))
            if session_id.as_str() == "fleet-missing-session"
    ));
    new_client.close();
    new_server.await.expect("fleet daemon peer");
}

/// MUTATION CHECK: advance the cursor across a gap, narrow raw payloads to
/// known EventPayload kinds, or reattach from zero. Expected RUNTIME failure:
/// output becomes `[1,3]`, the additive object is lost, or the second attach's
/// literal `after_seq` differs from 1.
#[tokio::test]
async fn watch_recovers_exactly_after_gap_and_forwards_additive_raw_envelopes() {
    let (_root, profile) = profile();
    let listener = match UnixListener::bind(&profile.endpoint_path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            // Some hermetic runners deny AF_UNIX binds. Ordinary macOS CI
            // executes the full cursor scenario; do not replace it with a
            // weaker in-memory framing test here.
            return;
        }
        Err(error) => panic!("bind observe peer: {error}"),
    };
    let session_id = SessionId::new("observe-session");
    let server_profile = profile.clone();
    let server_session = session_id.clone();
    let server = tokio::spawn(async move {
        let mut first = accept_peer(&listener, welcome(&server_profile, "observe-first")).await;
        let first_attachment =
            accept_attach(&mut first, &server_session, 0, "observe-attachment-1").await;
        send_event(
            &mut first,
            &first_attachment,
            &server_session,
            raw(&server_session, 1, "first"),
        )
        .await;
        send_event(
            &mut first,
            &first_attachment,
            &server_session,
            raw(&server_session, 3, "gap"),
        )
        .await;
        drop(first);

        let mut second = accept_peer(&listener, welcome(&server_profile, "observe-second")).await;
        let second_attachment =
            accept_attach(&mut second, &server_session, 1, "observe-attachment-2").await;
        send_event(
            &mut second,
            &second_attachment,
            &server_session,
            raw(&server_session, 2, "second"),
        )
        .await;
        send_event(
            &mut second,
            &second_attachment,
            &server_session,
            raw(&server_session, 3, "third"),
        )
        .await;
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id: second_attachment,
                high_water_seq: 3,
            })
            .await;
    });

    let (sender, mut receiver) = mpsc::unbounded_channel();
    let result = tokio::time::timeout(
        BOUND,
        observe_stream_session(&profile, false, session_id, false, sender),
    )
    .await
    .expect("observe stream deadline");
    result.expect("observe stream succeeds");
    tokio::time::timeout(BOUND, server)
        .await
        .expect("peer scenario deadline")
        .expect("peer scenario");

    let envelopes = drain_ended_stream(&mut receiver);
    assert_eq!(
        envelopes
            .iter()
            .map(|envelope| envelope.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(envelopes[2].payload["type"], "future_observe_kind_v9");
    assert_eq!(envelopes[2].payload["additive"]["kept"], true);
}

/// MUTATION CHECK: capture the replay-loss baseline after attach instead of
/// before it. Expected RUNTIME failure: the first burst drops its caught-up
/// marker without a visible loss delta and the five-second deadline expires.
#[tokio::test]
async fn replay_overflow_during_attach_is_detected_and_resumed() {
    // Must exceed the client's bounded uncorrelated-frame lane
    // (`EVENT_CAPACITY` in client.rs, 256) so the pre-response burst below
    // overflows it. If the lane ever outgrows this head, the client finishes
    // on the first connection and the bounded peer-scenario join fails loudly.
    const REPLAY_HEAD: u64 = 400;

    let (_root, profile) = profile();
    let listener = match UnixListener::bind(&profile.endpoint_path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("bind observe overflow peer: {error}"),
    };
    let session_id = SessionId::new("observe-overflow-session");
    let server_profile = profile.clone();
    let server_session = session_id.clone();
    let server = tokio::spawn(async move {
        let mut first = accept_peer(&listener, welcome(&server_profile, "overflow-first")).await;
        let (request_id, body) = first.request().await;
        let RequestBody::SessionAttach {
            session_id: attached,
            after_seq,
            mode,
        } = body
        else {
            panic!("expected first overflow attach");
        };
        assert_eq!(attached, server_session);
        assert_eq!(after_seq, 0);
        assert_eq!(mode, AttachMode::View);
        let first_attachment = AttachmentId::new("overflow-attachment-1");
        // The replay and its caught-up marker go on the wire BEFORE the
        // attach response: the client cannot drain uncorrelated frames while
        // its attach request is unresolved, so a replay longer than the
        // bounded event lane overflows it DETERMINISTICALLY and drops the
        // caught-up marker — the exact loss the pre-attach baseline must
        // expose. (A real daemon answers first, but this is the same client
        // state as one session's replay flooding while a later session's
        // attach is still pending. A response-first burst only overflows
        // when the scheduler starves the drain — a coin flip that let the
        // client finish on the first connection and wedge this fixture's
        // second accept forever.)
        let mut burst = Vec::new();
        for seq in 1..=REPLAY_HEAD {
            burst.push(WireFrame::Event {
                attachment_id: first_attachment.clone(),
                session_id: server_session.clone(),
                envelope: raw(&server_session, seq, "overflow-first"),
            });
        }
        burst.push(WireFrame::AttachCaughtUp {
            attachment_id: first_attachment.clone(),
            high_water_seq: REPLAY_HEAD,
        });
        burst.push(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionAttach {
                attachment_id: first_attachment,
                attach_state: AttachState {
                    session_id: server_session.clone(),
                    requested_after_seq: 0,
                    replay_through_seq: REPLAY_HEAD,
                    worker_generation: 7,
                    authority_epoch: 1,
                },
            },
        });
        first.write_batch(&burst).await;
        drop(first);

        let mut second = accept_peer(&listener, welcome(&server_profile, "overflow-second")).await;
        let (request_id, body) = second.request().await;
        let RequestBody::SessionAttach {
            session_id: attached,
            after_seq,
            mode,
        } = body
        else {
            panic!("expected resumed overflow attach");
        };
        assert_eq!(attached, server_session);
        assert!(after_seq < REPLAY_HEAD);
        assert_eq!(mode, AttachMode::View);
        let second_attachment = AttachmentId::new("overflow-attachment-2");
        second
            .respond(
                request_id,
                ResponseBody::SessionAttach {
                    attachment_id: second_attachment.clone(),
                    attach_state: AttachState {
                        session_id: server_session.clone(),
                        requested_after_seq: after_seq,
                        replay_through_seq: REPLAY_HEAD,
                        worker_generation: 7,
                        authority_epoch: 1,
                    },
                },
            )
            .await;
        for seq in after_seq.saturating_add(1)..=REPLAY_HEAD {
            send_event(
                &mut second,
                &second_attachment,
                &server_session,
                raw(&server_session, seq, "overflow-resumed"),
            )
            .await;
            if seq % 64 == 0 {
                tokio::task::yield_now().await;
            }
        }
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id: second_attachment,
                high_water_seq: REPLAY_HEAD,
            })
            .await;
    });

    let (sender, mut receiver) = mpsc::unbounded_channel();
    tokio::time::timeout(
        BOUND,
        observe_stream_session(&profile, false, session_id, false, sender),
    )
    .await
    .expect("overflow replay deadline")
    .expect("overflow replay succeeds");
    tokio::time::timeout(BOUND, server)
        .await
        .expect("overflow peer scenario deadline")
        .expect("overflow peer scenario");

    let sequences = drain_ended_stream(&mut receiver)
        .iter()
        .map(|envelope| envelope.seq)
        .collect::<Vec<_>>();
    assert_eq!(sequences, (1..=REPLAY_HEAD).collect::<Vec<_>>());
}

/// The watch contract for permanent peer loss: when the daemon is gone for
/// good (endpoint refuses), the stream must END with a typed error after
/// bounded dial attempts — never hang the CLI silently. The CLI maps this
/// typed end onto the headless exit-code table (`NoDaemon` → EX_UNAVAILABLE).
///
/// MUTATION CHECK: re-enable unbounded dial retry on a refused endpoint in
/// `stream_shard`, or retain a clone of the caller's output sender past the
/// stream future. Expected RUNTIME failure: the five-second stream deadline
/// expires, or the ended-stream drain observes a still-live sender.
#[tokio::test]
async fn permanent_daemon_loss_ends_the_watch_stream_with_a_typed_error() {
    let (_root, profile) = profile();
    let listener = match UnixListener::bind(&profile.endpoint_path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("bind observe loss peer: {error}"),
    };
    let session_id = SessionId::new("observe-loss-session");
    let server_profile = profile.clone();
    let server_session = session_id.clone();
    let server = tokio::spawn(async move {
        let mut peer = accept_peer(&listener, welcome(&server_profile, "loss-only")).await;
        let attachment = accept_attach(&mut peer, &server_session, 0, "loss-attachment-1").await;
        send_event(
            &mut peer,
            &attachment,
            &server_session,
            raw(&server_session, 1, "before-loss"),
        )
        .await;
        send_event(
            &mut peer,
            &attachment,
            &server_session,
            raw(&server_session, 2, "before-loss"),
        )
        .await;
        // Permanent daemon death: the socket node stays behind, dials get
        // ECONNREFUSED. The listener falls first so the connection EOF the
        // client observes already implies the endpoint refuses.
        drop(listener);
        drop(peer);
    });

    let (sender, mut receiver) = mpsc::unbounded_channel();
    let result = tokio::time::timeout(
        BOUND,
        observe_stream_session(&profile, false, session_id, true, sender),
    )
    .await
    .expect("permanent loss must end the watch stream within its deadline");
    match result {
        Err(ObserveError::NoDaemon(_)) => {}
        other => panic!("expected the typed NoDaemon stream end, got {other:?}"),
    }
    tokio::time::timeout(BOUND, server)
        .await
        .expect("loss peer scenario deadline")
        .expect("loss peer scenario");

    let sequences = drain_ended_stream(&mut receiver)
        .iter()
        .map(|envelope| envelope.seq)
        .collect::<Vec<_>>();
    assert_eq!(sequences, [1, 2]);
}
