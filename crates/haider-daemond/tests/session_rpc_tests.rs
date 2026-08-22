//! End-to-end UDS coverage for the W3b2 session hub and fair outbox.

#![allow(clippy::expect_used)]

mod support;

use haider_daemon::DaemonConfig;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_rpc::{
    AttachMode, ClientKind, RequestBody, RequestId, ResponseBody, SeqRange, WireFrame,
};
use haider_store::{EventStore, Store};
use support::{UdsClient as Client, ready, test_root};

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

/// MUTATION CHECK: route list/read through attachment registration or start
/// replay before enqueueing the attach response. Expected failure: an
/// unsolicited event follows list/read, or an event precedes the attachment
/// ID response.
#[tokio::test]
async fn uds_session_lifecycle_lists_reads_attaches_replays_and_detaches() {
    let root = test_root("w3b2-rpc-");
    let config = DaemonConfig::new(
        "session-lifecycle",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let session_id = SessionId::new("session-a");
    seed(&config, &session_id, 3);
    let task = ready(&config).await;
    let mut client = Client::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3b2-test",
        "client",
        ClientKind::Gui,
    )
    .await;

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
        client.next_reply().await,
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
        client.next_reply().await,
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
                    sealed_replay: false,
                },
            },
            config.frame_limit,
        )
        .await;
    let attachment_id = match client.next_reply().await {
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
        client.next_reply().await,
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
    // This must exceed the platform UDS send buffer so the first replay
    // cannot finish before the server consumes the already-sent cold attach.
    // Linux buffers the former 200 tiny frames in full, which never creates
    // the two simultaneously active lanes whose fairness this test proves.
    const HOT_EVENTS: usize = 2_000;

    let root = test_root("w3b2-rpc-");
    let mut config = DaemonConfig::new(
        "fair-attachments",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    config.outbound_queue_capacity = 6;
    let hot = SessionId::new("hot-session");
    let cold = SessionId::new("cold-session");
    seed(&config, &hot, HOT_EVENTS);
    seed(&config, &cold, 1);
    let task = ready(&config).await;
    let mut client = Client::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3b2-test",
        "client",
        ClientKind::Gui,
    )
    .await;

    for (request_id, session_id) in [("hot", hot.clone()), ("cold", cold.clone())] {
        client
            .send(
                &WireFrame::Request {
                    request_id: RequestId::new(request_id),
                    body: RequestBody::SessionAttach {
                        session_id,
                        after_seq: 0,
                        mode: AttachMode::View,
                        sealed_replay: false,
                    },
                },
                config.frame_limit,
            )
            .await;
    }
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response { .. }
    ));

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
            WireFrame::AttachCaughtUp { high_water_seq, .. }
                if high_water_seq == HOT_EVENTS as u64 =>
            {
                break;
            }
            WireFrame::Lagged { .. } => {
                panic!("a continuously reading client must never be lagged")
            }
            WireFrame::Response { .. } => {}
            frame => panic!("unexpected fairness frame: {frame:?}"),
        }
        if !cold_caught_up {
            assert!(
                hot_events < HOT_EVENTS as u64,
                "round-robin must serve the cold lane before the hot replay completes"
            );
        }
    }
    assert!(cold_caught_up, "cold attachment fully served");
    assert_eq!(
        hot_events, HOT_EVENTS as u64,
        "hot attachment fully served, unlagged"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// MUTATION CHECK: treat the sink's `Busy` admission as a hard refusal (the
/// pre-W3b2.3 behavior of an unpaced burst hitting the bound). Expected
/// failure: the first page overruns the 16-frame lane quota and this
/// continuously READING client is purged through `Lagged` instead of
/// receiving the contiguous 1..=1000 stream.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn paced_replay_of_a_long_history_never_laggs_a_reading_client() {
    let root = test_root("w3b2-rpc-");
    let config = DaemonConfig::new(
        "paced-replay",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let session_id = SessionId::new("long-history");
    seed(&config, &session_id, 1_000);
    let task = ready(&config).await;
    let mut client = Client::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3b2-test",
        "client",
        ClientKind::Gui,
    )
    .await;
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("attach-long"),
                body: RequestBody::SessionAttach {
                    session_id,
                    after_seq: 0,
                    mode: AttachMode::View,
                    sealed_replay: false,
                },
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response { .. }
    ));

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

fn seed_with_payload(config: &DaemonConfig, session_id: &SessionId, count: usize, blob: usize) {
    let store = Store::open(&config.store_dir).expect("seed store");
    let mut events = (1..=count)
        .map(|seq| {
            let mut event = envelope(session_id, &format!("{}-{seq}", session_id.as_str()));
            event.payload = serde_json::json!({
                "type": "future_rpc_seed",
                "blob": "x".repeat(blob),
            });
            event
        })
        .collect::<Vec<_>>();
    store.append(&mut events).expect("seed append");
}

/// MUTATION CHECK: admit on the frame dimension only (ignore bytes in the
/// sink's atomic offer, so a byte-bound admission refuses instead of
/// answering `Busy`). Expected failure: a few large replay envelopes fill
/// the byte budget and this continuously READING client is purged through
/// `Lagged` mid-replay.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn byte_bound_replay_of_large_envelopes_never_laggs_a_reading_client() {
    let root = test_root("w3b2-rpc-");
    let mut config = DaemonConfig::new(
        "byte-bound-replay",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    config.frame_limit = 128 * 1024;
    config.outbound_queued_bytes = 256 * 1024;
    let session_id = SessionId::new("large-envelopes");
    seed_with_payload(&config, &session_id, 30, 60 * 1024);
    let task = ready(&config).await;
    let mut client = Client::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3b2-test",
        "client",
        ClientKind::Gui,
    )
    .await;
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("attach-large"),
                body: RequestBody::SessionAttach {
                    session_id,
                    after_seq: 0,
                    mode: AttachMode::View,
                    sealed_replay: false,
                },
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response { .. }
    ));

    let mut next_seq = 1_u64;
    loop {
        match client.next().await {
            WireFrame::Response { .. } => {}
            WireFrame::Event { envelope, .. } => {
                assert_eq!(envelope.seq, next_seq, "replay must stay contiguous");
                next_seq += 1;
            }
            WireFrame::AttachCaughtUp {
                high_water_seq: 30, ..
            } => break,
            WireFrame::Lagged { .. } => {
                panic!("a continuously reading client must never be lagged")
            }
            frame => panic!("unexpected byte-bound frame: {frame:?}"),
        }
    }
    assert_eq!(next_seq, 31, "every large envelope arrived exactly once");

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// MUTATION CHECK: treat the sink's `Busy` admission as a hard refusal —
/// equivalent to the reverted capacity SNAPSHOT, under which three or more
/// concurrent replay lanes jointly observe the same aggregate headroom and
/// the overbooked loser is purged. Expected failure: at least one of the
/// five lanes on this one connection receives `Lagged` although the client
/// reads continuously.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn five_concurrent_replay_lanes_on_one_connection_never_lag_a_reading_client() {
    let root = test_root("w3b2-rpc-");
    let mut config = DaemonConfig::new(
        "five-lanes",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    // Aggregate 6 frames across 5 active lanes: admission contention is
    // constant, so any snapshot-shaped admission overbooks immediately.
    config.outbound_queue_capacity = 6;
    let sessions = (0..5)
        .map(|index| SessionId::new(format!("lane-{index}")))
        .collect::<Vec<_>>();
    for session_id in &sessions {
        seed(&config, session_id, 30);
    }
    let task = ready(&config).await;
    let mut client = Client::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3b2-test",
        "client",
        ClientKind::Gui,
    )
    .await;
    for (index, session_id) in sessions.iter().enumerate() {
        client
            .send(
                &WireFrame::Request {
                    request_id: RequestId::new(format!("attach-{index}")),
                    body: RequestBody::SessionAttach {
                        session_id: session_id.clone(),
                        after_seq: 0,
                        mode: AttachMode::View,
                        sealed_replay: false,
                    },
                },
                config.frame_limit,
            )
            .await;
    }
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response { .. }
    ));

    let mut caught_up = 0_usize;
    let mut next_seq: std::collections::HashMap<SessionId, u64> = sessions
        .iter()
        .map(|session_id| (session_id.clone(), 1_u64))
        .collect();
    while caught_up < sessions.len() {
        match client.next().await {
            WireFrame::Response { .. } => {}
            WireFrame::Event {
                session_id,
                envelope,
                ..
            } => {
                let expected = next_seq.get_mut(&session_id).expect("known session");
                assert_eq!(envelope.seq, *expected, "each lane stays contiguous");
                *expected += 1;
            }
            WireFrame::AttachCaughtUp {
                high_water_seq: 30, ..
            } => caught_up += 1,
            WireFrame::Lagged { .. } => {
                panic!("no reading lane may be purged by admission contention")
            }
            frame => panic!("unexpected five-lane frame: {frame:?}"),
        }
    }
    for (session_id, expected) in next_seq {
        assert_eq!(expected, 31, "lane {session_id} delivered all 30 events");
    }

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
