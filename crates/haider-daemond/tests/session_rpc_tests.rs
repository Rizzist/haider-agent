//! End-to-end UDS coverage for the W3b2 session hub and fair outbox.

#![allow(clippy::expect_used)]

use haider_daemon::{DaemonConfig, DaemonState, spawn};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_rpc::{
    AttachMode, Capability, CapabilitySet, ClientKind, Hello, RequestBody, RequestId, ResponseBody,
    SeqRange, WIRE_PROTOCOL_VERSION, WireFrame, uds_codec,
};
use haider_store::{EventStore, Store};
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const DEADLINE: Duration = Duration::from_secs(10);

fn test_root() -> tempfile::TempDir {
    #[cfg(target_os = "macos")]
    const SHORT_TMP_ROOT: &str = "/private/tmp";
    #[cfg(not(target_os = "macos"))]
    const SHORT_TMP_ROOT: &str = "/tmp";

    tempfile::Builder::new()
        .prefix("w3b2-rpc-")
        .tempdir_in(SHORT_TMP_ROOT)
        .expect("short temp root")
}

fn envelope(session_id: &SessionId, event_id: &str) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("rpc-seed"),
        authority_epoch: 4,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({"type": "future_rpc_seed"}),
    }
}

fn seed(config: &DaemonConfig, session_id: &SessionId, count: usize) {
    let store = Store::open(&config.store_dir).expect("seed store");
    let mut events = (1..=count)
        .map(|seq| envelope(session_id, &format!("{}-{seq}", session_id.as_str())))
        .collect::<Vec<_>>();
    store.append(&mut events).expect("seed append");
}

async fn ready(config: &DaemonConfig) -> haider_daemon::DaemonTask {
    let task = spawn(config.clone());
    let mut readiness = task.readiness();
    tokio::time::timeout(DEADLINE, async {
        loop {
            if readiness.current() == DaemonState::Ready {
                return;
            }
            readiness
                .changed()
                .await
                .expect("daemon state remains open");
        }
    })
    .await
    .expect("ready deadline");
    task
}

struct Client {
    stream: UnixStream,
    decoder: uds_codec::Decoder,
    pending: VecDeque<WireFrame>,
}

impl Client {
    async fn connect(path: &Path, frame_limit: usize) -> Self {
        let mut client = Self {
            stream: UnixStream::connect(path).await.expect("connect"),
            decoder: uds_codec::Decoder::new(frame_limit),
            pending: VecDeque::new(),
        };
        client
            .send(
                &WireFrame::Hello(Hello {
                    protocol_min: WIRE_PROTOCOL_VERSION,
                    protocol_max: WIRE_PROTOCOL_VERSION,
                    client_name: "w3b2-test".into(),
                    client_version: "test".into(),
                    client_instance_id: "client".into(),
                    client_kind: ClientKind::Gui,
                    capabilities_requested: CapabilitySet::from([
                        Capability::View,
                        Capability::Control,
                    ]),
                    max_receive_frame: u32::try_from(frame_limit).expect("frame limit"),
                }),
                frame_limit,
            )
            .await;
        assert!(matches!(client.next().await, WireFrame::Welcome(_)));
        client
    }

    async fn send(&mut self, frame: &WireFrame, frame_limit: usize) {
        let bytes = uds_codec::encode(frame, frame_limit).expect("frame encodes");
        self.stream.write_all(&bytes).await.expect("frame writes");
    }

    async fn next(&mut self) -> WireFrame {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if let Some(frame) = self.pending.pop_front() {
                    return frame;
                }
                let mut bytes = [0_u8; 16 * 1024];
                let read = self.stream.read(&mut bytes).await.expect("frame reads");
                assert_ne!(read, 0, "connection closed before expected frame");
                let batch = self.decoder.push(&bytes[..read]);
                assert!(batch.error.is_none(), "invalid server frame");
                self.pending.extend(batch.frames);
            }
        })
        .await
        .expect("frame deadline")
    }
}

/// MUTATION CHECK: route list/read through attachment registration or start
/// replay before enqueueing the attach response. Expected failure: an
/// unsolicited event follows list/read, or an event precedes the attachment
/// ID response.
#[tokio::test]
async fn uds_session_lifecycle_lists_reads_attaches_replays_and_detaches() {
    let root = test_root();
    let config = DaemonConfig::new(
        "session-lifecycle",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let session_id = SessionId::new("session-a");
    seed(&config, &session_id, 3);
    let task = ready(&config).await;
    let mut client = Client::connect(&config.endpoint_path(), config.frame_limit).await;

    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("list"),
                body: RequestBody::SessionList {
                    cursor: None,
                    limit: 10,
                },
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        client.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionList { ref sessions, .. },
            ..
        } if sessions.len() == 1 && sessions[0].session_id == session_id
    ));
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("read"),
                body: RequestBody::SessionRead {
                    session_id: session_id.clone(),
                    range: SeqRange {
                        start_seq: 2,
                        end_seq: 3,
                    },
                },
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        client.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionRead { ref result },
            ..
        } if result.envelopes.iter().map(|event| event.seq).collect::<Vec<_>>() == [2, 3]
    ));
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("attach"),
                body: RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 1,
                    mode: AttachMode::View,
                },
            },
            config.frame_limit,
        )
        .await;
    let attachment_id = match client.next().await {
        WireFrame::Response {
            body:
                ResponseBody::SessionAttach {
                    attachment_id,
                    attach_state,
                },
            ..
        } => {
            assert_eq!(attach_state.replay_through_seq, 3);
            attachment_id
        }
        frame => panic!("expected attach response, got {frame:?}"),
    };
    assert!(matches!(
        client.next().await,
        WireFrame::Event { ref envelope, .. } if envelope.seq == 2
    ));
    assert!(matches!(
        client.next().await,
        WireFrame::Event { ref envelope, .. } if envelope.seq == 3
    ));
    assert!(matches!(
        client.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 3,
            ..
        }
    ));
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("detach"),
                body: RequestBody::SessionDetach {
                    attachment_id: attachment_id.clone(),
                },
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        client.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionDetach { attachment_id: found },
            ..
        } if found == attachment_id
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// MUTATION CHECK: remove the per-attachment quota/round-robin key and
/// enqueue every event in one FIFO lane. Expected failure: the cold
/// attachment's caught-up marker no longer precedes the hot replay's
/// completion. Companion mutation: restore the unpaced page bursts —
/// expected failure: the hot READING client is purged through `Lagged` and
/// the no-lag assertion below trips.
#[tokio::test]
async fn one_hot_attachment_cannot_starve_a_cold_attachment_on_the_same_connection() {
    let root = test_root();
    let mut config = DaemonConfig::new(
        "fair-attachments",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    config.outbound_queue_capacity = 6;
    let hot = SessionId::new("hot-session");
    let cold = SessionId::new("cold-session");
    seed(&config, &hot, 200);
    seed(&config, &cold, 1);
    let task = ready(&config).await;
    let mut client = Client::connect(&config.endpoint_path(), config.frame_limit).await;

    for (request_id, session_id) in [("hot", hot.clone()), ("cold", cold.clone())] {
        client
            .send(
                &WireFrame::Request {
                    request_id: RequestId::new(request_id),
                    body: RequestBody::SessionAttach {
                        session_id,
                        after_seq: 0,
                        mode: AttachMode::View,
                    },
                },
                config.frame_limit,
            )
            .await;
    }

    let mut cold_caught_up = false;
    let mut hot_events = 0_u64;
    loop {
        match client.next().await {
            WireFrame::Event {
                session_id,
                envelope,
                ..
            } if session_id == cold => assert_eq!(envelope.seq, 1),
            WireFrame::Event {
                session_id,
                envelope,
                ..
            } if session_id == hot => {
                hot_events += 1;
                assert_eq!(envelope.seq, hot_events);
            }
            WireFrame::AttachCaughtUp {
                high_water_seq: 1, ..
            } => cold_caught_up = true,
            WireFrame::AttachCaughtUp {
                high_water_seq: 200,
                ..
            } => break,
            WireFrame::Lagged { .. } => {
                panic!("a continuously reading client must never be lagged")
            }
            WireFrame::Response { .. } => {}
            frame => panic!("unexpected fairness frame: {frame:?}"),
        }
        if !cold_caught_up {
            assert!(
                hot_events < 200,
                "round-robin must serve the cold lane before the hot replay completes"
            );
        }
    }
    assert!(cold_caught_up, "cold attachment fully served");
    assert_eq!(hot_events, 200, "hot attachment fully served, unlagged");

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// MUTATION CHECK: revert replay pacing — deliver each store page
/// synchronously, ignoring `capacity_for`/`drain_progress`. Expected failure:
/// the first page overruns the 16-frame lane quota and this continuously
/// READING client is purged through `Lagged` instead of receiving the
/// contiguous 1..=1000 stream.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn paced_replay_of_a_long_history_never_laggs_a_reading_client() {
    let root = test_root();
    let config = DaemonConfig::new(
        "paced-replay",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let session_id = SessionId::new("long-history");
    seed(&config, &session_id, 1_000);
    let task = ready(&config).await;
    let mut client = Client::connect(&config.endpoint_path(), config.frame_limit).await;
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("attach-long"),
                body: RequestBody::SessionAttach {
                    session_id,
                    after_seq: 0,
                    mode: AttachMode::View,
                },
            },
            config.frame_limit,
        )
        .await;

    let mut next_seq = 1_u64;
    loop {
        match client.next().await {
            WireFrame::Response { .. } => {}
            WireFrame::Event { envelope, .. } => {
                assert_eq!(envelope.seq, next_seq, "replay must stay contiguous");
                next_seq += 1;
            }
            WireFrame::AttachCaughtUp {
                high_water_seq: 1_000,
                ..
            } => break,
            WireFrame::Lagged { .. } => {
                panic!("a continuously reading client must never be lagged")
            }
            frame => panic!("unexpected paced-replay frame: {frame:?}"),
        }
    }
    assert_eq!(
        next_seq, 1_001,
        "every seeded envelope arrived exactly once"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
