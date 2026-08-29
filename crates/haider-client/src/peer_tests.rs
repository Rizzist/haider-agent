#![allow(clippy::expect_used)]

use super::peer::{
    PeerEvent, peer_event_from_frame, peer_list_response, peer_messaging_available,
    peer_name_response, peer_send_response,
};
use haider_rpc::haider_protocol::peer::{
    PeerDelivery, PeerDescriptor, PeerKind, PeerMessage, PeerReceipt, PeerSender, PeerState,
    PeerTrust,
};
use haider_rpc::{
    CapabilitySet, FEATURE_PEER_MESSAGING_V1, LifecyclePhase, ResponseBody, Welcome, WireFrame,
};

#[test]
fn feature_absence_makes_the_peer_surface_absent_without_an_error() {
    let mut welcome = Welcome {
        protocol: 1,
        instance_id: "instance".into(),
        daemon_generation: 1,
        frame_limit: 1024,
        profile_id: "profile".into(),
        daemon_version: "test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::default(),
        features: Default::default(),
        user_command_withheld: false,
        encoding: None,
    };
    assert!(!peer_messaging_available(&welcome));
    welcome.features.insert(FEATURE_PEER_MESSAGING_V1.into());
    assert!(peer_messaging_available(&welcome));
}

#[test]
fn typed_peer_responses_preserve_contract_fields() {
    let descriptor = PeerDescriptor {
        id: "peer-1".into(),
        name: "reviewer".into(),
        kind: PeerKind::External,
        workspace: "/work".into(),
        model: "review-model".into(),
        state: PeerState::Busy,
        started_at: 10,
        last_seen: 20,
    };
    assert_eq!(
        peer_list_response(ResponseBody::PeerList {
            agents: vec![descriptor.clone()]
        })
        .expect("peer list"),
        std::slice::from_ref(&descriptor)
    );
    let receipt = PeerReceipt {
        msg_id: "msg-1".into(),
        delivery: PeerDelivery::Queued,
        reason: None,
    };
    assert_eq!(
        peer_send_response(ResponseBody::PeerSend {
            receipt: receipt.clone()
        })
        .expect("peer send"),
        receipt
    );
    let renamed = PeerDescriptor {
        name: "builder".into(),
        ..descriptor
    };
    assert_eq!(
        peer_name_response(ResponseBody::PeerName {
            agent: renamed.clone()
        })
        .expect("peer name"),
        renamed
    );
}

#[test]
fn received_and_delivery_frames_map_to_typed_subscription_events() {
    let message = PeerMessage {
        msg_id: "msg-1".into(),
        from: PeerSender {
            id: "peer-1".into(),
            name: "reviewer".into(),
            kind: PeerKind::External,
            trust: PeerTrust::UntrustedExternal,
        },
        to: "session-1".into(),
        message: "review this".into(),
        summary: None,
        queued_at: 10,
        expires_at: 20,
    };
    assert_eq!(
        peer_event_from_frame(WireFrame::PeerMessageReceived {
            message: message.clone()
        }),
        Some(PeerEvent::Received(message))
    );
    let receipt = PeerReceipt {
        msg_id: "msg-1".into(),
        delivery: PeerDelivery::Delivered,
        reason: None,
    };
    assert_eq!(
        peer_event_from_frame(WireFrame::PeerDeliveryChanged {
            receipt: receipt.clone()
        }),
        Some(PeerEvent::DeliveryChanged(receipt))
    );
}
