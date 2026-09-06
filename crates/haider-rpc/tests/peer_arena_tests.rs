#![allow(clippy::expect_used)]

use bytes::Bytes;
use haider_rpc::{RequestBody, WireFrame, uds_codec};

fn request(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "v":1,"kind":"request","request_id":"peer-injection",
        "body":{"method":"peer.inject","message":{
            "msg_id":"message-1","from":{"id":"sender","device_id":"device","name":"teammate","kind":"haider_session","trust":"untrusted_external","mode":"prompting"},
            "to":"target","message":text,"queued_at":1,"expires_at":0
        }}
    })).expect("wire request")
}

#[test]
fn peer_rpc_adopts_unescaped_input_as_an_arena_range() {
    let body = Bytes::from(request("a distinct teammate message"));
    let range = body.as_ptr() as usize..body.as_ptr() as usize + body.len();
    let frame = uds_codec::decode_owned_json(body, 128 * 1024).expect("decode owned bytes");
    let WireFrame::Request {
        body: RequestBody::PeerInject { message },
        ..
    } = frame
    else {
        panic!("peer injection");
    };
    message
        .message
        .visit_strs(|part| assert!(range.contains(&(part.as_ptr() as usize))));
    let cloned = message.clone();
    let mut original = 0;
    let mut duplicate = 1;
    message
        .message
        .visit_strs(|part| original = part.as_ptr() as usize);
    cloned
        .message
        .visit_strs(|part| duplicate = part.as_ptr() as usize);
    assert_eq!(
        original, duplicate,
        "the transcript clone must retain the arena"
    );
}

#[test]
fn peer_rpc_escaped_input_and_ordinary_framing_remain_equivalent() {
    for text in ["plain", "line\nquote\"\\é<"] {
        let body = Bytes::from(request(text));
        let expected: WireFrame = serde_json::from_slice(&body).expect("ordinary decode");
        let actual = uds_codec::decode_owned_json(body.clone(), body.len()).expect("owned decode");
        assert_eq!(actual, expected);
        assert!(uds_codec::decode_owned_json(body.clone(), body.len() - 1).is_err());
    }
    let ping = Bytes::from_static(br#"{"v":1,"kind":"ping","nonce":17}"#);
    assert_eq!(
        uds_codec::decode_owned_json(ping, 128).expect("ping"),
        WireFrame::Ping { nonce: 17 }
    );
    assert!(uds_codec::decode_owned_json(Bytes::from_static(b"{"), 128).is_err());
}
