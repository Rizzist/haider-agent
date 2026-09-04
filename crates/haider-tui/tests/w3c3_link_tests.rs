#![cfg(unix)]
//! W3c3 M2 — the live IO shell (`link.rs`) against a fake daemon peer.
//!
//! `LiveDriver` is pure and pinned without a socket; the LINK is the half
//! that owns ordering, and ordering only exists relative to a peer. So these
//! tests speak `uds_codec` frames over a real `UnixListener` and control the
//! exact order the daemon's bytes arrive in — which is the whole point of
//! every law below:
//!
//! * an attach RESPONSE reaches the driver before the first event for the
//!   attachment it names, even when the events overtook it on the wire
//!   (review W3c3 P1-2 — an event for an unknown attachment id is rejected
//!   and lost forever);
//! * `[Detach, Attach]` reaches the WIRE in that order, or the daemon
//!   rejects the attach at `max_attachments_per_connection` (P1-3);
//! * a frame the CLIENT dropped under backpressure becomes a reported gap,
//!   never silence (P1-2 — `lost_events()` had no caller);
//! * `AttachCaughtUp` becomes the catch-up boundary instead of being
//!   discarded (P1-2).
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use haider_client::{ClientConfig, ProfileEnv, ResolvedProfile, connect, resolve_profile};
use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, MenuId, SessionId};
use haider_rpc::{
    AttachMode, AttachState, AttachmentId, Capability, CapabilitySet, CommandId,
    DEFAULT_FRAME_LIMIT, LifecyclePhase, ProtocolError, RequestBody, ResponseBody,
    SubmitDisposition, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
};
use haider_tui::link::{
    CommandContext, Link, map_frame, map_response, request_body, request_body_for_features,
};
use haider_tui::live::{LiveCommand, LiveReply};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const LIMIT: usize = DEFAULT_FRAME_LIMIT;
/// Every wire await is bounded: a wedged link must fail the suite, not hang
/// it.
const BOUND: Duration = Duration::from_secs(20);

// ------------------------------------------------------------- fake peer --

fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hlink")
        .tempdir_in("/tmp")
        .expect("short temp dir")
}

fn welcome() -> Welcome {
    Welcome {
        protocol: WIRE_PROTOCOL_VERSION,
        instance_id: "fake-instance".into(),
        daemon_generation: 1,
        frame_limit: LIMIT as u32,
        profile_id: "profile-link".into(),
        daemon_version: "0.0.1-fake".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features: BTreeSet::new(),
        user_command_withheld: false,
        encoding: None,
    }
}

fn encoded(frame: &WireFrame) -> Vec<u8> {
    uds_codec::encode(frame, LIMIT).expect("encode fake frame")
}

async fn write_frame(stream: &mut UnixStream, frame: &WireFrame) {
    stream
        .write_all(&encoded(frame))
        .await
        .expect("write fake frame");
}

/// Reads until at least one frame decodes; empty means the peer hung up.
async fn read_frames(stream: &mut UnixStream, decoder: &mut uds_codec::Decoder) -> Vec<WireFrame> {
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return Vec::new();
        };
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

/// A fake daemon: `Hello` → `Welcome`, then `serve` owns the connection.
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

/// A profile whose endpoint is the fake peer's socket, so a redial (which
/// these tests never provoke) would go somewhere sane.
fn profile_for(store: &Path, endpoint: &Path) -> ResolvedProfile {
    let env = ProfileEnv {
        profile_dir: Some(store.to_path_buf()),
        home: None,
        user_profile: None,
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    };
    let mut profile = resolve_profile(&env).expect("resolve profile");
    profile.endpoint_path = endpoint.to_path_buf();
    profile
}

async fn link_to(store: &Path, endpoint: &Path) -> Link {
    let connected = connect(endpoint, ClientConfig::default())
        .await
        .expect("connect fake peer");
    Link::start(
        connected.client,
        profile_for(store, endpoint),
        ClientConfig::default(),
    )
}

async fn next_reply(link: &mut Link) -> LiveReply {
    tokio::time::timeout(BOUND, link.replies.recv())
        .await
        .expect("the link must produce a reply")
        .expect("the link must stay alive")
}

// ------------------------------------------------------------- fixtures --

fn session(n: usize) -> SessionId {
    SessionId::new(format!("s-{n}"))
}

fn attachment(n: usize) -> AttachmentId {
    AttachmentId::new(format!("att-{n}"))
}

fn envelope(session: &SessionId, seq: u64) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("link-device"),
        authority_epoch: 1,
        worker_generation: 9,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({ "type": "noop", "seq": seq }).into(),
    }
}

fn attach_state(session: &SessionId, replay_through_seq: u64) -> AttachState {
    AttachState {
        session_id: session.clone(),
        requested_after_seq: 0,
        replay_through_seq,
        worker_generation: 9,
        authority_epoch: 1,
    }
}

/// The wire identity of a request body, for wire-order assertions.
fn label(body: &RequestBody) -> String {
    match body {
        RequestBody::SessionDetach { attachment_id } => {
            format!("detach:{}", attachment_id.as_str())
        }
        RequestBody::SessionAttach { session_id, .. } => {
            format!("attach:{}", session_id.as_str())
        }
        other => format!("other:{other:?}"),
    }
}

// ------------------------------------------------ 1. the attach barrier --

/// THE LAW: the attach RESPONSE precedes the first event for its attachment
/// IN THE REPLY STREAM, whatever order the bytes arrived in.
///
/// The daemon's register→replay seam puts the response first on the wire,
/// but the link cannot rely on that and did not preserve it either: the
/// response travelled through a spawned task while events travelled through
/// the select loop, so they raced. This peer writes the adversarial order —
/// two events and the caught-up marker BEFORE the response, in ONE socket
/// write so they decode as one batch — and the reply stream must still open
/// with `Attached`. An event that arrives first is rejected by the driver as
/// an unknown attachment id and is gone for good.
///
/// MUTATION CHECK: in `run_link`, replace the `deliver(reply, ...)` call in
/// the `events.recv()` arm with `replies.send(reply).await.is_ok()` (i.e.
/// delete the barrier). Expected failure: the first reply is
/// `LiveReply::Event { .. }` and the `Attached` assertion below panics with
/// "the attach response must be published first".
#[tokio::test]
async fn attach_response_precedes_events_that_overtook_it_on_the_wire() {
    let dir = short_dir();
    let endpoint = dir.path().join("barrier.sock");
    let _peer = spawn_fake_peer(&endpoint, |mut stream, mut decoder| async move {
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
                    WireFrame::Request {
                        request_id,
                        body: RequestBody::SessionAttach { session_id, .. },
                    } => {
                        let att = attachment(1);
                        let mut batch = Vec::new();
                        for seq in 1..=2 {
                            batch.extend(encoded(&WireFrame::Event {
                                attachment_id: att.clone(),
                                session_id: session_id.clone(),
                                envelope: envelope(&session_id, seq),
                            }));
                        }
                        batch.extend(encoded(&WireFrame::AttachCaughtUp {
                            attachment_id: att.clone(),
                            high_water_seq: 2,
                        }));
                        batch.extend(encoded(&WireFrame::Response {
                            request_id,
                            body: ResponseBody::SessionAttach {
                                attachment_id: att,
                                attach_state: attach_state(&session_id, 2),
                            },
                        }));
                        stream.write_all(&batch).await.expect("write batch");
                    }
                    _ => {}
                }
            }
        }
    });

    let mut link = link_to(dir.path(), &endpoint).await;
    link.commands
        .send(LiveCommand::Attach {
            session: session(1),
            after_seq: 0,
        })
        .await
        .expect("link accepts commands");

    match next_reply(&mut link).await {
        LiveReply::Attached {
            session: got,
            attachment: att,
            replay_through_seq,
            ..
        } => {
            assert_eq!(got, session(1));
            assert_eq!(att, attachment(1));
            assert_eq!(replay_through_seq, 2);
        }
        other => panic!("the attach response must be published first, got {other:?}"),
    }
    // The held replies then flush IN ORDER — holding must not reorder or
    // deduplicate, only delay.
    for seq in 1..=2 {
        match next_reply(&mut link).await {
            LiveReply::Event {
                attachment: att,
                envelope,
                ..
            } => {
                assert_eq!(att, attachment(1));
                assert_eq!(envelope.seq, seq, "held events flush in wire order");
            }
            other => panic!("expected the held event {seq}, got {other:?}"),
        }
    }
    match next_reply(&mut link).await {
        LiveReply::CaughtUp {
            attachment: att,
            high_water_seq,
        } => {
            assert_eq!(att, attachment(1));
            assert_eq!(high_water_seq, 2);
        }
        other => panic!("expected the catch-up boundary, got {other:?}"),
    }
}

// -------------------------------------------------- 2. ordered wire send --

/// How many eviction pairs to push through. One pair cannot prove an
/// ordering that is only PROBABLY wrong when broken; a run of them can.
const PAIRS: usize = 64;

/// THE LAW: the driver's `[Detach, Attach]` eviction pair reaches the WIRE
/// in that order.
///
/// The driver emits both in one ordered vector precisely because the daemon
/// caps attachments per connection: the detach frees the slot the attach
/// needs. Performing each on its own task made both race to
/// `RpcClient::request` and therefore to the outbound queue, and a daemon
/// that sees the attach first answers `max_attachments_per_connection`
/// (review W3c3 P1-3). The fix awaits the SEND inline and spawns only the
/// wait, so this is the observable: 64 pairs, no inversion.
///
/// MUTATION CHECK: revert `issue` to a `fn` whose whole body is
/// `tokio::spawn(async move { ... client.request(body).await ... })` and
/// call it without `.await` from `run_link`. Expected failure: the recorded
/// wire order contains at least one `attach:s-N` before its `detach:att-N`
/// and the `assert_eq!` on the full sequence fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detach_then_attach_reaches_the_wire_in_that_order() {
    let dir = short_dir();
    let endpoint = dir.path().join("order.sock");
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
                match frame {
                    WireFrame::Ping { nonce } => {
                        write_frame(&mut stream, &WireFrame::Pong { nonce }).await;
                    }
                    WireFrame::Request { request_id, body } => {
                        if let Ok(mut seen) = recorder.lock() {
                            seen.push(label(&body));
                            if seen.len() == PAIRS * 2
                                && let Some(done) = done.take()
                            {
                                let _ = done.send(());
                            }
                        }
                        let response = match body {
                            RequestBody::SessionDetach { attachment_id } => {
                                ResponseBody::SessionDetach { attachment_id }
                            }
                            RequestBody::SessionAttach { session_id, .. } => {
                                ResponseBody::SessionAttach {
                                    attachment_id: AttachmentId::new(format!(
                                        "live-{}",
                                        session_id.as_str()
                                    )),
                                    attach_state: attach_state(&session_id, 0),
                                }
                            }
                            _ => continue,
                        };
                        write_frame(
                            &mut stream,
                            &WireFrame::Response {
                                request_id,
                                body: response,
                            },
                        )
                        .await;
                    }
                    _ => {}
                }
            }
        }
    });

    let link = link_to(dir.path(), &endpoint).await;
    let mut expected = Vec::with_capacity(PAIRS * 2);
    for n in 0..PAIRS {
        link.commands
            .send(LiveCommand::Detach {
                attachment: attachment(n),
            })
            .await
            .expect("link accepts the detach");
        link.commands
            .send(LiveCommand::Attach {
                session: session(n),
                after_seq: 0,
            })
            .await
            .expect("link accepts the attach");
        expected.push(format!("detach:att-{n}"));
        expected.push(format!("attach:s-{n}"));
    }
    tokio::time::timeout(BOUND, done_rx)
        .await
        .expect("the peer must observe every frame")
        .expect("recorder alive");
    let order = seen.lock().expect("recorder").clone();
    assert_eq!(
        order, expected,
        "every detach must reach the daemon before the attach that reuses its slot"
    );
}

/// The executing source guard on the same law. The inversion window above is
/// a scheduler race, so a passing run is evidence and not proof; this pins
/// the SHAPE that makes the race impossible — the send is awaited before the
/// wait is spawned.
///
/// MUTATION CHECK: move `client.begin_request(body).await` inside the
/// `tokio::spawn` block in `issue`. Expected failure: the position assertion
/// below inverts.
#[test]
fn issue_awaits_the_send_before_it_spawns_the_wait() {
    let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/link.rs"))
        .expect("link source");
    let start = source.find("async fn issue").expect("issue must be async");
    let body = &source[start..];
    let send = body
        .find("begin_request(body).await")
        .expect("the send half is awaited inside issue");
    let spawn = body.find("tokio::spawn").expect("the wait half is spawned");
    assert!(
        send < spawn,
        "the ordered send must complete before the concurrent wait is spawned"
    );
    assert!(
        source.contains("issue(&client, command, &replies, &attaches_tx).await"),
        "run_link must await issue inline, or command order is a scheduler coin flip"
    );
}

// ------------------------------------------------- 3. dropped-event gaps --

/// THE LAW: a frame the CLIENT dropped becomes a reported gap.
///
/// `route_frame` drops uncorrelated frames when the event channel is full
/// and counts them, so response correlation can never be blocked by a slow
/// consumer. Nothing read that counter, which made a dropped FINAL envelope
/// pure silence — a hole in the middle is exposed by the next sequence, a
/// hole at the end is exposed by nothing (review W3c3 P1-2). The link now
/// polls the counter and turns any increase into `EventsLost`, which the
/// driver answers by reattaching every attachment from its own cursor.
///
/// The flood below deliberately outruns both bounded channels: the reply
/// channel is not drained until after the peer has written far more frames
/// than the client's event channel can hold.
///
/// MUTATION CHECK: delete the `client.lost_events()` probe block at the top
/// of `run_link`'s loop. Expected failure: the ~512 buffered events drain,
/// no `EventsLost` ever follows them, and `next_reply` below panics on its
/// `BOUND` timeout ("the link must produce a reply: Elapsed") — which is
/// precisely the silence the probe exists to break.
#[tokio::test]
async fn client_side_event_drops_surface_as_events_lost() {
    const FLOOD: usize = 3_000;
    let dir = short_dir();
    let endpoint = dir.path().join("flood.sock");
    let _peer = spawn_fake_peer(&endpoint, |mut stream, mut decoder| async move {
        let att = attachment(7);
        let sid = session(7);
        for seq in 1..=FLOOD as u64 {
            write_frame(
                &mut stream,
                &WireFrame::Event {
                    attachment_id: att.clone(),
                    session_id: sid.clone(),
                    envelope: envelope(&sid, seq),
                },
            )
            .await;
        }
        // Stay up so the link never sees a disconnect.
        loop {
            if read_frames(&mut stream, &mut decoder).await.is_empty() {
                return;
            }
        }
    });

    let mut link = link_to(dir.path(), &endpoint).await;
    // Do not read a single reply until the flood has overrun both bounded
    // channels; that overrun is what makes the client drop frames at all.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut events = 0_usize;
    let lost = loop {
        match next_reply(&mut link).await {
            LiveReply::Event { .. } => {
                events += 1;
                assert!(
                    events <= FLOOD,
                    "a client-side drop must surface as EventsLost"
                );
            }
            LiveReply::EventsLost { count } => break count,
            other => panic!("unexpected reply during the flood: {other:?}"),
        }
    };
    assert!(lost > 0, "the reported gap must name at least one frame");
    assert!(
        events + usize::try_from(lost).unwrap_or(usize::MAX) <= FLOOD,
        "the link must not invent frames the daemon never sent"
    );
}

// ------------------------------------------------ 4. pure mapping laws --

/// THE LAW: `AttachCaughtUp` is the catch-up boundary, not noise.
///
/// It used to fall through `map_frame`'s catch-all under a comment claiming
/// it "carries no state the cursor law needs". It carries the only state
/// that can expose a replay whose LAST envelopes were lost.
///
/// MUTATION CHECK: delete the `WireFrame::AttachCaughtUp` arm from
/// `map_frame` so it falls through to the catch-all. Expected failure: this
/// test sees an empty reply vector.
#[test]
fn attach_caught_up_maps_to_the_catch_up_boundary() {
    let replies = map_frame(WireFrame::AttachCaughtUp {
        attachment_id: attachment(3),
        high_water_seq: 41,
    });
    assert_eq!(
        replies,
        vec![LiveReply::CaughtUp {
            attachment: attachment(3),
            high_water_seq: 41,
        }],
        "the high water mark must reach the driver or a truncated replay is invisible"
    );
}

/// The rest of the uncorrelated frames the link translates, plus the
/// forward-compat law: an unknown frame maps to nothing and is never fatal.
#[test]
fn map_frame_translates_every_uncorrelated_frame_it_owns() {
    let sid = session(2);
    assert_eq!(
        map_frame(WireFrame::Event {
            attachment_id: attachment(2),
            session_id: sid.clone(),
            envelope: envelope(&sid, 5),
        }),
        vec![LiveReply::Event {
            attachment: attachment(2),
            session: sid.clone(),
            envelope: Box::new(envelope(&sid, 5)),
        }]
    );
    assert_eq!(
        map_frame(WireFrame::Lagged {
            attachment_id: attachment(2),
            // Server telemetry, deliberately NOT carried: the driver resumes
            // from its own cursor (R9's cursor law).
            last_queued_seq: 99,
        }),
        vec![LiveReply::Lagged {
            attachment: attachment(2)
        }]
    );
    assert_eq!(
        map_frame(WireFrame::ServerDraining {
            reason: "upgrade".into(),
            instance_id: "i-1".into(),
            daemon_generation: 4,
            deadline_unix_ms: 1,
        }),
        vec![LiveReply::Draining {
            reason: "upgrade".into()
        }]
    );
    assert_eq!(
        map_frame(WireFrame::ProtocolError(ProtocolError {
            code: "overloaded".into(),
            message: "try later".into(),
            fatal: false,
            presentation: None,
            failed_write_ids: Vec::new(),
        })),
        vec![LiveReply::Failed {
            command_id: None,
            code: "overloaded".into(),
            message: "try later".into(),
            // A non-fatal protocol error is retryable; a fatal one is not.
            retryable: true,
            presentation: None,
        }]
    );
    assert!(
        map_frame(WireFrame::Pong { nonce: 1 }).is_empty(),
        "frames the link does not own map to nothing, never to a panic"
    );
}

/// THE LAW: an attach asks for CONTROL from the greatest sequence the
/// reducer fully applied, and a detach names exactly the attachment being
/// released. Both are the wire half of the eviction pair above.
#[test]
fn request_body_round_trips_the_attachment_commands() {
    assert_eq!(
        request_body(LiveCommand::Attach {
            session: session(4),
            after_seq: 12,
        }),
        RequestBody::SessionAttach {
            session_id: session(4),
            after_seq: 12,
            mode: AttachMode::Control,
            sealed_replay: false,
        }
    );
    assert_eq!(
        request_body(LiveCommand::Detach {
            attachment: attachment(4),
        }),
        RequestBody::SessionDetach {
            attachment_id: attachment(4),
        }
    );
    assert_eq!(
        request_body(LiveCommand::List { cursor: None }),
        RequestBody::SessionList {
            cursor: None,
            limit: haider_tui::live::LIST_PAGE,
            order: Default::default(),
        }
    );
    let recency_features = BTreeSet::from([haider_rpc::FEATURE_SESSION_LIST_RECENCY_V1.to_owned()]);
    assert_eq!(
        request_body_for_features(LiveCommand::List { cursor: None }, &recency_features),
        RequestBody::SessionList {
            cursor: None,
            limit: haider_tui::live::LIST_PAGE,
            order: haider_rpc::SessionListOrderWire::RecencyDesc,
        },
        "a feature-serving daemon gets newest-first list requests"
    );
    assert_eq!(
        request_body(LiveCommand::Cancel {
            command_id: CommandId::new("cmd-1"),
            session: session(4),
            worker_generation: 9,
            run_id: haider_protocol::ids::RunId::new("run-1"),
            branch: None,
        }),
        RequestBody::TurnCancel {
            command_id: CommandId::new("cmd-1"),
            session_id: session(4),
            worker_generation: 9,
            run_id: haider_protocol::ids::RunId::new("run-1"),
        }
    );
}

/// THE LAW: an attach response carries no session id of its own request's
/// choosing, and an attach FAILURE carries no durable command id at all — so
/// both are interpreted through the context captured at send time. Lose that
/// and a wedged session is unattachable for the life of the connection
/// (review P1-5).
#[test]
fn map_response_interprets_attach_outcomes_through_their_context() {
    let context = CommandContext::of(&LiveCommand::Attach {
        session: session(5),
        after_seq: 0,
    });
    assert_eq!(
        map_response(
            &context,
            ResponseBody::SessionAttach {
                attachment_id: attachment(5),
                attach_state: attach_state(&session(5), 30),
            },
        ),
        vec![LiveReply::Attached {
            session: session(5),
            attachment: attachment(5),
            worker_generation: 9,
            replay_through_seq: 30,
        }]
    );
    assert_eq!(
        map_response(
            &context,
            ResponseBody::Error {
                code: "overloaded".into(),
                message: "busy".into(),
                retryable: true,
                data: None,
            },
        ),
        vec![LiveReply::AttachFailed {
            session: session(5),
            code: "overloaded".into(),
            message: "busy".into(),
            retryable: true,
        }],
        "an attach failure must name its session; there is no command id to correlate by"
    );
    // The same error body, from a command that DOES have a durable id, is a
    // plain failure — the two must never be confused.
    let submit = CommandContext::of(&LiveCommand::Submit {
        command_id: CommandId::new("cmd-2"),
        session: session(5),
        worker_generation: 9,
        text: "hi".into(),
        mode: DeliveryMode::Steer,
        branch: None,
        attachments: vec![],
    });
    assert_eq!(
        map_response(
            &submit,
            ResponseBody::Error {
                code: "stale_generation".into(),
                message: "restart".into(),
                retryable: false,
                data: None,
            },
        ),
        vec![LiveReply::Failed {
            command_id: Some(CommandId::new("cmd-2")),
            code: "stale_generation".into(),
            message: "restart".into(),
            retryable: false,
            presentation: None,
        }]
    );
    assert_eq!(
        map_response(
            &submit,
            ResponseBody::TurnSubmit {
                session_id: session(5),
                worker_generation: 9,
                run_id: haider_protocol::ids::RunId::new("run-2"),
                accepted_seq: 4,
                disposition: SubmitDisposition::Started,
            },
        ),
        vec![LiveReply::Submitted {
            command_id: CommandId::new("cmd-2"),
            session: session(5),
            worker_generation: 9,
            disposition: SubmitDisposition::Started,
        }]
    );
    assert_eq!(
        map_response(&submit, ResponseBody::MenuAnswer { resolution_seq: 3 }),
        Vec::new(),
        "an unmapped body is tolerated, never fatal (forward-compat law)"
    );
}

/// THE LAW: a menu answer rides `send_frame` as an UNCORRELATED frame. Its
/// durable identity is `command_id` and its outcome is the committed
/// `MenuAnswered` envelope; a correlated echo would be a second authority
/// over which answer won.
///
/// MUTATION CHECK: route `LiveCommand::Answer` through `request_body`
/// instead. Expected failure: `request_body` hits its `unreachable!` and
/// this test panics — and the wire gains a `request_id` the CAS never asked
/// for.
#[tokio::test]
async fn menu_answers_reach_the_wire_uncorrelated() {
    let dir = short_dir();
    let endpoint = dir.path().join("menu.sock");
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<WireFrame>();
    let _peer = spawn_fake_peer(&endpoint, |mut stream, mut decoder| async move {
        let mut seen = Some(seen_tx);
        loop {
            let frames = read_frames(&mut stream, &mut decoder).await;
            if frames.is_empty() {
                return;
            }
            for frame in frames {
                if matches!(frame, WireFrame::MenuAnswer { .. })
                    && let Some(seen) = seen.take()
                {
                    let _ = seen.send(frame);
                }
            }
        }
    });
    let link = link_to(dir.path(), &endpoint).await;
    link.commands
        .send(LiveCommand::Answer {
            command_id: CommandId::new("cmd-menu"),
            session: session(6),
            menu: MenuId::new("menu-1"),
            request_seq: 8,
            worker_generation: 9,
            option_key: "yes".into(),
            option_index: 0,
            input: None,
        })
        .await
        .expect("link accepts the answer");
    let frame = tokio::time::timeout(BOUND, seen_rx)
        .await
        .expect("the answer must reach the wire")
        .expect("peer alive");
    match frame {
        WireFrame::MenuAnswer {
            request_id,
            command_id,
            request_seq,
            worker_generation,
            ..
        } => {
            assert_eq!(request_id, None, "a menu answer is never correlated");
            assert_eq!(command_id, CommandId::new("cmd-menu"));
            // The CAS fence: the committed opening coordinates, verbatim.
            assert_eq!(request_seq, 8);
            assert_eq!(worker_generation, 9);
        }
        other => panic!("expected a MenuAnswer, got {other:?}"),
    }
}

// ------------------------------------------------- 4. barrier overflow --

/// THE LAW (review r2 NF-1): loss the barrier swallows while an attach is
/// OUTSTANDING is deferred — never published mid-barrier. An `EventsLost`
/// published before the attach outcome installs reaches a driver with no
/// attachment to repair; the later `Attached` then paints a surface whose
/// replay and catch-up boundary were silently thrown away, and a quiescent
/// session never exposes the hole.
///
/// This is a SEAM test on `link::deliver` itself: a socket-driven flood
/// cannot deterministically overflow the barrier because the client's own
/// bounded event channel drops first (a different loss class). The flush
/// half — deferred loss published only after the barrier drains — is pinned
/// by the source guard below it.
///
/// MUTATION CHECK: revert `deliver` to `held.clear()` +
/// `replies.send(LiveReply::EventsLost { .. })` inline on overflow (the
/// pre-NF-1 body). Expected failure: `try_recv` observes an `EventsLost`
/// mid-barrier and the "nothing published" assertion fails.
#[tokio::test]
async fn barrier_overflow_defers_loss_instead_of_publishing_mid_barrier() {
    use haider_tui::link::{HELD_REPLY_CAP, deliver};
    use std::collections::VecDeque;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<LiveReply>(HELD_REPLY_CAP * 2 + 8);
    let mut held: VecDeque<LiveReply> = VecDeque::new();
    let mut deferred: u64 = 0;
    let event = |seq: u64| LiveReply::Event {
        session: session(1),
        attachment: attachment(1),
        envelope: Box::new(envelope(&session(1), seq)),
    };
    // Fill to the cap, then two more: the cap+1th clears-and-defers, the
    // cap+2th re-accumulates in the (now empty) queue.
    let flood = u64::try_from(HELD_REPLY_CAP).expect("cap fits") + 2;
    for seq in 1..=flood {
        assert!(deliver(event(seq), 1, &mut held, &mut deferred, &tx).await);
    }
    assert!(
        rx.try_recv().is_err(),
        "nothing may publish while the attach is outstanding — loss defers"
    );
    assert_eq!(
        deferred,
        u64::try_from(HELD_REPLY_CAP).expect("cap fits") + 1,
        "the deferral coalesces the cleared queue plus the overflowing reply"
    );
    assert_eq!(held.len(), 1, "the queue re-accumulates after the clear");

    // With the barrier down, delivery is direct and the deferral untouched.
    assert!(deliver(event(flood + 1), 0, &mut held, &mut deferred, &tx).await);
    assert!(matches!(rx.try_recv(), Ok(LiveReply::Event { .. })));
    assert_eq!(
        deferred,
        u64::try_from(HELD_REPLY_CAP).expect("cap fits") + 1
    );
}

/// The flush half of NF-1, pinned at the source: the deferred-loss publish
/// lives INSIDE the barrier-drain block (`outstanding_attaches == 0`),
/// AFTER the held flush — so the loss report always follows the attach
/// outcomes and the surviving held tail it refers to.
///
/// MUTATION CHECK: move the `deferred_loss > 0` publish above the held
/// flush (or out of the drain block). Expected failure: the position
/// assertions below invert.
#[test]
fn deferred_loss_publishes_only_after_the_barrier_drains() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/link.rs"),
    )
    .expect("link source");
    let drain = source
        .find("if outstanding_attaches == 0 {")
        .expect("the barrier-drain block");
    let flush = source[drain..]
        .find("while let Some(reply) = held.pop_front()")
        .expect("the held flush inside the drain block")
        + drain;
    let publish = source[drain..]
        .find("if deferred_loss > 0 {")
        .expect("the deferred publish inside the drain block")
        + drain;
    assert!(
        flush < publish,
        "the deferred loss must publish AFTER the held tail it refers to"
    );
    // And the deferral itself must not publish inline: the overflow arm of
    // `deliver` contains no send.
    let overflow = source
        .find("held.len() >= HELD_REPLY_CAP")
        .expect("the overflow arm");
    let arm_end = source[overflow..]
        .find("held.push_back(reply);")
        .expect("the hold arm follows")
        + overflow;
    assert!(
        !source[overflow..arm_end].contains("replies.send"),
        "the overflow arm defers; it must not publish mid-barrier"
    );
}

/// MUTATION CHECK: drop the models-provider context tag from the error
/// mapping. Expected RUNTIME failure: a `provider.models_refresh` error
/// maps to the generic `Failed` flash instead of the row-scoped reply
/// (the owner's boot-time `provider_error` launcher flash).
#[test]
fn models_refresh_error_maps_to_the_row_scoped_reply() {
    let context = CommandContext::of(&LiveCommand::RefreshProviderModels {
        provider: "probefix".into(),
    });
    let replies = map_response(
        &context,
        haider_rpc::ResponseBody::Error {
            code: "provider_error".into(),
            message: "provider does not expose a subscription model catalog".into(),
            retryable: false,
            data: None,
        },
    );
    assert!(
        matches!(
            replies.as_slice(),
            [LiveReply::ModelsRefreshFailed { provider, .. }] if provider == "probefix"
        ),
        "got {replies:?}"
    );
}

/// MUTATION CHECK: drop the automode overrides from the session-create
/// mapping. Expected RUNTIME failure: the assertion below (owner
/// directive: the interactive surface never opens write/exec approval
/// menus).
#[test]
fn tui_session_create_carries_automode_overrides() {
    let body = request_body(LiveCommand::Create {
        command_id: haider_rpc::CommandId::new("automode-create"),
        cwd: "/tmp".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        first_text: "hi".into(),
    });
    match body {
        haider_rpc::RequestBody::SessionCreateWithPermissionOverrides {
            permission_overrides: Some(overrides),
            interaction_mode,
            ..
        } => {
            assert!(overrides.allow_writes && overrides.allow_exec);
            assert_eq!(
                interaction_mode,
                haider_protocol::session::SessionInteractionModeV1::Interactive
            );
        }
        other => panic!("create must carry automode overrides, got {other:?}"),
    }
}
