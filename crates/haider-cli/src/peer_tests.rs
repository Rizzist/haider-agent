#![allow(clippy::expect_used)]

use super::{
    PEER_EVENT_SCHEMA, PEER_LIST_SCHEMA, PeerCommand, PeerEventDocument, PeerListDocument,
    parse_peer_command,
};
use haider_client::{
    PeerDelivery, PeerDescriptor, PeerKind, PeerMessage, PeerReceipt, PeerSender, PeerState,
    PeerTrust,
};

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn peer_cli_parser_pins_all_surfaces() {
    assert_eq!(
        parse_peer_command(&args(&["list", "--json"])),
        Ok(PeerCommand::List { json: true })
    );
    assert_eq!(
        parse_peer_command(&args(&["send", "reviewer", "inspect this"])),
        Ok(PeerCommand::Send {
            to: "reviewer".into(),
            message: "inspect this".into(),
        })
    );
    assert_eq!(
        parse_peer_command(&args(&["name", "builder"])),
        Ok(PeerCommand::Name {
            name: "builder".into()
        })
    );
    assert_eq!(
        parse_peer_command(&args(&["watch"])),
        Ok(PeerCommand::Watch)
    );
}

#[test]
fn peer_json_contract_shapes_are_golden() {
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
    let list = serde_json::to_value(PeerListDocument {
        schema: PEER_LIST_SCHEMA,
        agents: &[descriptor],
    })
    .expect("list serializes");
    assert_eq!(
        list,
        serde_json::json!({
            "schema": "haider.peer.list.v1",
            "agents": [{
                "id": "peer-1",
                "name": "reviewer",
                "kind": "external",
                "workspace": "/work",
                "model": "review-model",
                "state": "busy",
                "started_at": 10,
                "last_seen": 20
            }]
        })
    );

    let message = PeerMessage {
        msg_id: "msg-1".into(),
        from: PeerSender {
            id: "peer-1".into(),
            name: "reviewer".into(),
            kind: PeerKind::External,
            trust: PeerTrust::UntrustedExternal,
        },
        to: "builder".into(),
        message: "inspect this".into(),
        summary: None,
        queued_at: 30,
        expires_at: 40,
    };
    let event = serde_json::to_value(PeerEventDocument::Received {
        schema: PEER_EVENT_SCHEMA,
        message: &message,
    })
    .expect("event serializes");
    assert_eq!(
        event,
        serde_json::json!({
            "kind": "received",
            "schema": "haider.peer.event.v1",
            "message": {
                "msg_id": "msg-1",
                "from": {
                    "id": "peer-1",
                    "name": "reviewer",
                    "kind": "external",
                    "trust": "untrusted_external"
                },
                "to": "builder",
                "message": "inspect this",
                "queued_at": 30,
                "expires_at": 40
            }
        })
    );

    let receipt = PeerReceipt {
        msg_id: "msg-1".into(),
        delivery: PeerDelivery::Delivered,
        reason: None,
    };
    let event = serde_json::to_value(PeerEventDocument::DeliveryChanged {
        schema: PEER_EVENT_SCHEMA,
        receipt: &receipt,
    })
    .expect("delivery event serializes");
    assert_eq!(
        event,
        serde_json::json!({
            "kind": "delivery_changed",
            "schema": "haider.peer.event.v1",
            "receipt": {
                "msg_id": "msg-1",
                "delivery": "delivered"
            }
        })
    );
}
