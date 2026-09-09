#![allow(clippy::expect_used)]

use super::*;
use crate::peer_tests::{live_peer_fixture, start_held_peer_turn};
use haider_core::StoreHandle;
use haider_protocol::EventPayload;
use haider_protocol::ids::RunId;
use haider_protocol::peer::{PeerDeliveryState, PeerSendOptions, PeerStatusQuery};

fn options(id: &str) -> PeerSendOptions {
    PeerSendOptions {
        msg_id: Some(id.into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn peer_offline_queue_survives_sender_and_receiver_restart_and_replays_once() {
    let root = tempfile::tempdir_in("/tmp").expect("short root");
    let runtime = root.path().join("r");
    let sender_root = root.path().join("sender");
    let target_root = root.path().join("target");
    let (sh, ss, sender) = live_peer_fixture(&sender_root, &runtime, "sender").await;
    let (th, ts, target) = live_peer_fixture(&target_root, &runtime, "target").await;
    let target_address = target
        .list()
        .await
        .expect("list")
        .into_iter()
        .find(|p| p.id == "target")
        .expect("target")
        .address();
    target.shutdown().await;
    th.shutdown().await.expect("target stop");
    ts.close().await.expect("target store close");
    let from = SessionId::new("sender");
    let held = sender
        .send_with_options(
            &from,
            target_address.clone(),
            "queued offline".into(),
            None,
            options("stable-msg"),
        )
        .await
        .expect("accepted offline");
    assert_eq!(
        held.status.as_ref().expect("durable status").state,
        PeerDeliveryState::Held
    );
    assert!(
        held.status
            .as_ref()
            .expect("status")
            .reason
            .as_ref()
            .expect("connect reason")
            .contains("endpoint")
    );
    assert!(
        !sh.daemon_is_durably_quiescent()
            .await
            .expect("pending quiescence")
    );
    sender.shutdown().await;
    sh.shutdown().await.expect("sender stop");
    ss.close().await.expect("sender store close");
    let (sh, ss, sender) = live_peer_fixture(&sender_root, &runtime, "sender").await;
    assert_eq!(sender.outbox.lock().await.len(), 1);
    assert!(
        !sh.daemon_is_durably_quiescent()
            .await
            .expect("recovered quiescence")
    );
    let (th, ts, target) = live_peer_fixture(&target_root, &runtime, "target").await;
    let target_id = SessionId::new("target");
    let manager = start_held_peer_turn(&th, &ts, &target_id, &RunId::new("busy-target")).await;
    sender.drain_outbox().await.expect("reconnect drain");
    assert!(sender.outbox.lock().await.is_empty());
    assert!(
        sh.daemon_is_durably_quiescent()
            .await
            .expect("delivered quiescence")
    );
    let receipt = sender
        .send_with_options(
            &from,
            target_address,
            "queued offline".into(),
            None,
            options("stable-msg"),
        )
        .await
        .expect("lost response replay");
    assert_eq!(receipt.msg_id, held.msg_id);
    assert_eq!(
        receipt.status.as_ref().expect("status").state,
        PeerDeliveryState::Delivered
    );
    assert_eq!(
        receipt.status.as_ref().expect("status").accepted_at_ms,
        held.status.as_ref().expect("status").accepted_at_ms
    );
    let sender_events = ss
        .read(&SessionId::new("sender"), 0, 256)
        .await
        .expect("sender journal");
    let mut replay_message = sender_events
        .iter()
        .find_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::PeerOutbox(entry)) => Some(entry.message),
            _ => None,
        })
        .expect("accepted outbox message");
    let sender_paths =
        peer_endpoint_paths(&runtime, "sender", PeerEndpointKind::Haider).expect("sender paths");
    let mut renamed: PeerManifest =
        serde_json::from_slice(&std::fs::read(&sender_paths.manifest).expect("sender manifest"))
            .expect("manifest");
    renamed.name = "renamed sender".into();
    renamed.state = PeerState::Busy;
    write_manifest_blocking(
        &sender_paths.manifest,
        &renamed,
        MANIFEST_CREATION_SYNC_POLICY,
    )
    .expect("rename registration");
    let target_paths =
        peer_endpoint_paths(&runtime, "target", PeerEndpointKind::Haider).expect("target paths");
    let replay = exchange_delivery(
        &target_paths.socket,
        replay_message.clone(),
        &th.peer_device_id(),
    )
    .await
    .expect("duplicate wire replay after renamed sender");
    assert_eq!(
        replay.status.expect("status").state,
        PeerDeliveryState::Delivered
    );
    replay_message.message = "conflicting body".into();
    assert!(
        exchange_delivery(
            &target_paths.socket,
            replay_message.clone(),
            &th.peer_device_id()
        )
        .await
        .is_err()
    );
    replay_message.from.id = "target".into();
    assert!(
        exchange_delivery(&target_paths.socket, replay_message, &th.peer_device_id())
            .await
            .is_err(),
        "socket sender cannot claim receiver's local identity"
    );
    assert_eq!(
        th.queue_snapshot(target_id.clone())
            .await
            .expect("queue")
            .rows
            .len(),
        1
    );
    let page = sender
        .delivery_status(PeerStatusQuery {
            session_id: from,
            msg_id: Some("stable-msg".into()),
            after_seq: 0,
        })
        .await
        .expect("status replay");
    assert_eq!(
        page.receipts
            .iter()
            .map(|r| r.status.as_ref().expect("status").state)
            .collect::<Vec<_>>(),
        [
            PeerDeliveryState::Accepted,
            PeerDeliveryState::Held,
            PeerDeliveryState::Delivered
        ]
    );
    let events = ts.read(&target_id, 0, 256).await.expect("receiver journal");
    assert_eq!(events.iter().filter(|event| matches!(event.payload.decode_event(), Ok(EventPayload::NodeCommitted(node)) if matches!(node.kind, haider_protocol::history::NodeKind::Agent { .. }))).count(), 1);
    sender.shutdown().await;
    target.shutdown().await;
    manager.shutdown().await.expect("worker stop");
    sh.shutdown().await.expect("sender stop");
    th.shutdown().await.expect("target stop");
    ss.close().await.expect("sender close");
    ts.close().await.expect("target close");
}

#[tokio::test]
async fn peer_outbox_bounds_expiry_cancel_conflict_and_removed_registration() {
    let root = tempfile::tempdir_in("/tmp").expect("short root");
    let runtime = root.path().join("r");
    let (sh, ss, sender) = live_peer_fixture(&root.path().join("s"), &runtime, "sender").await;
    let (th, ts, target) = live_peer_fixture(&root.path().join("t"), &runtime, "target").await;
    let from = SessionId::new("sender");
    target.shutdown().await;
    for index in 0..delivery::MAX_PER_RECIPIENT {
        let held = sender
            .send_with_options(
                &from,
                "target".into(),
                "hello".into(),
                None,
                options(&format!("bounded-{index}")),
            )
            .await
            .expect("queue entry");
        assert_eq!(held.status.expect("status").state, PeerDeliveryState::Held);
    }
    let full = sender
        .send_with_options(
            &from,
            "target".into(),
            "hello".into(),
            None,
            options("full"),
        )
        .await
        .expect("full receipt");
    assert_eq!(
        full.status.expect("status").state,
        PeerDeliveryState::Failed
    );
    assert!(
        sender
            .send_with_options(
                &from,
                "target".into(),
                "different".into(),
                None,
                options("bounded-0")
            )
            .await
            .is_err()
    );
    let cancelled = sender
        .send_with_options(
            &from,
            String::new(),
            String::new(),
            None,
            PeerSendOptions {
                cancel: true,
                ..options("bounded-0")
            },
        )
        .await
        .expect("cancel");
    assert_eq!(
        cancelled.status.expect("status").state,
        PeerDeliveryState::Failed
    );
    let expires = sender
        .send_with_options(
            &from,
            "target".into(),
            "expire".into(),
            None,
            PeerSendOptions {
                ttl_ms: Some(1),
                ..options("expires")
            },
        )
        .await
        .expect("expiring entry");
    tokio::time::sleep(Duration::from_millis(2)).await;
    sender.drain_outbox().await.expect("expiry drain");
    let expired = sender.prior_receipt_for_test(&from, &expires.msg_id).await;
    assert_eq!(
        expired.status.expect("status").state,
        PeerDeliveryState::Failed
    );
    let paths = peer_endpoint_paths(&runtime, "target", PeerEndpointKind::Haider).expect("paths");
    std::fs::remove_file(paths.manifest).expect("revoke registration");
    sender.drain_outbox().await.expect("revocation drain");
    assert!(sender.outbox.lock().await.is_empty());
    let error = sender
        .send(&from, "unknown".into(), "hello".into(), None)
        .await
        .expect_err("unknown fails");
    assert!(error.to_string().contains("candidates:"));
    sender.shutdown().await;
    sh.shutdown().await.expect("sender stop");
    th.shutdown().await.expect("target stop");
    ss.close().await.expect("sender close");
    ts.close().await.expect("target close");
}

#[tokio::test]
async fn peer_terminal_states_wake_and_release_durable_quiescence() {
    let root = tempfile::tempdir_in("/tmp").expect("short root");
    let runtime = root.path().join("r");
    let (sh, ss, sender) = live_peer_fixture(&root.path().join("s"), &runtime, "sender").await;
    let (th, ts, target) = live_peer_fixture(&root.path().join("t"), &runtime, "target").await;
    target.shutdown().await;
    let from = SessionId::new("sender");
    for terminal in ["cancel", "expire", "revoke"] {
        // Subscribe before admission: a one-millisecond TTL may legitimately
        // expire in the initial attempt on a loaded machine.
        let mut wake = sh.subscribe_peer_reconcile();
        let accepted = sender
            .send_with_options(
                &from,
                "target".into(),
                "pending".into(),
                None,
                PeerSendOptions {
                    ttl_ms: (terminal == "expire").then_some(1),
                    ..options(terminal)
                },
            )
            .await
            .expect("queue send");
        if terminal != "expire" {
            assert!(
                !sh.daemon_is_durably_quiescent()
                    .await
                    .expect("pending check")
            );
            while wake.try_recv().is_ok() {}
        }
        match terminal {
            "cancel" => {
                sender
                    .send_with_options(
                        &from,
                        String::new(),
                        String::new(),
                        None,
                        PeerSendOptions {
                            cancel: true,
                            ..options(terminal)
                        },
                    )
                    .await
                    .expect("cancel send");
            }
            "expire" => {
                let expiry = accepted
                    .status
                    .expect("accepted status")
                    .accepted_at_ms
                    .saturating_add(1);
                tokio::time::sleep(Duration::from_millis(
                    expiry.saturating_sub(now_ms()).saturating_add(1),
                ))
                .await;
                sender.drain_outbox().await.expect("expiry drain");
            }
            _ => {
                let paths = peer_endpoint_paths(&runtime, "target", PeerEndpointKind::Haider)
                    .expect("target paths");
                std::fs::remove_file(paths.manifest).expect("revoke target");
                sender.drain_outbox().await.expect("revocation drain");
            }
        }
        assert!(
            wake.try_recv().is_ok(),
            "{terminal} must wake idle retirement"
        );
        assert_eq!(
            sender
                .prior_receipt_for_test(&from, terminal)
                .await
                .status
                .expect("terminal status")
                .state,
            PeerDeliveryState::Failed
        );
        assert!(
            sh.daemon_is_durably_quiescent()
                .await
                .expect("terminal check"),
            "{terminal}"
        );
    }
    sender.shutdown().await;
    sh.shutdown().await.expect("sender stop");
    th.shutdown().await.expect("target stop");
    ss.close().await.expect("sender close");
    ts.close().await.expect("target close");
}

impl PeerService {
    async fn prior_receipt_for_test(&self, session: &SessionId, id: &str) -> PeerReceipt {
        let mut after_seq = 0;
        let mut receipt = None;
        loop {
            let page = self
                .delivery_status(PeerStatusQuery {
                    session_id: session.clone(),
                    msg_id: Some(id.into()),
                    after_seq,
                })
                .await
                .expect("status");
            for item in page.receipts {
                receipt = Some(item);
            }
            if !page.has_more {
                return receipt.expect("receipt exists");
            }
            after_seq = page.next_seq;
        }
    }
}
