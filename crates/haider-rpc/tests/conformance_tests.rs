//! Shared WS/UDS codec conformance and UDS stream framing cases.
#![allow(clippy::expect_used)]

mod common;

use common::{TEST_FRAME_LIMIT, transcript};
use haider_rpc::{CodecError, WireFrame, uds_codec, ws_codec};

struct CodecCase {
    name: &'static str,
    encode: fn(&WireFrame) -> Vec<u8>,
    decode: fn(&[u8]) -> WireFrame,
}

fn ws_encode(frame: &WireFrame) -> Vec<u8> {
    ws_codec::encode(frame, TEST_FRAME_LIMIT)
        .expect("WS encode")
        .into_bytes()
}

fn ws_decode(bytes: &[u8]) -> WireFrame {
    let text = std::str::from_utf8(bytes).expect("WS UTF-8");
    ws_codec::decode(text, TEST_FRAME_LIMIT).expect("WS decode")
}

fn uds_encode(frame: &WireFrame) -> Vec<u8> {
    uds_codec::encode(frame, TEST_FRAME_LIMIT).expect("UDS encode")
}

fn uds_decode(bytes: &[u8]) -> WireFrame {
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
    let frames = decoder.push(bytes).expect("UDS decode");
    assert_eq!(frames.len(), 1);
    frames.into_iter().next().expect("one frame")
}

#[test]
fn shared_transcript_is_parameterized_across_both_codecs() {
    let cases = [
        CodecCase {
            name: "websocket",
            encode: ws_encode,
            decode: ws_decode,
        },
        CodecCase {
            name: "uds",
            encode: uds_encode,
            decode: uds_decode,
        },
    ];

    for case in cases {
        for frame in transcript() {
            let first = (case.encode)(&frame);
            let second = (case.encode)(&frame);
            assert_eq!(first, second, "{} bytes were not deterministic", case.name);
            assert_eq!(
                (case.decode)(&first),
                frame,
                "{} round-trip mismatch",
                case.name
            );
        }
    }
}

#[test]
fn both_transports_use_identical_json_body_bytes() {
    for frame in transcript() {
        let ws = ws_codec::encode(&frame, TEST_FRAME_LIMIT).expect("WS encode");
        let uds = uds_codec::encode(&frame, TEST_FRAME_LIMIT).expect("UDS encode");
        let announced = u32::from_be_bytes(uds[..4].try_into().expect("prefix")) as usize;
        assert_eq!(announced, ws.len());
        assert_eq!(&uds[4..], ws.as_bytes());
    }
}

#[test]
fn uds_one_byte_drip_feed_yields_full_transcript() {
    let expected = transcript();
    let stream: Vec<u8> = expected
        .iter()
        .flat_map(|frame| uds_codec::encode(frame, TEST_FRAME_LIMIT).expect("encode"))
        .collect();
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
    let mut decoded = Vec::new();

    for byte in stream {
        decoded.extend(decoder.push(&[byte]).expect("drip decode"));
    }

    assert_eq!(decoded, expected);
}

#[test]
fn uds_coalesced_frames_are_all_yielded() {
    let frames = transcript();
    let mut chunk = uds_codec::encode(&frames[0], TEST_FRAME_LIMIT).expect("first");
    chunk.extend(uds_codec::encode(&frames[1], TEST_FRAME_LIMIT).expect("second"));

    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
    assert_eq!(decoder.push(&chunk).expect("coalesced decode"), frames[..2]);
}

#[test]
fn uds_split_length_prefix_is_buffered_without_body_allocation() {
    let frame = &transcript()[0];
    let bytes = uds_codec::encode(frame, TEST_FRAME_LIMIT).expect("encode");
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);

    assert!(
        decoder
            .push(&bytes[..2])
            .expect("first prefix half")
            .is_empty()
    );
    assert_eq!(
        decoder.push(&bytes[2..]).expect("rest of frame"),
        vec![frame.clone()]
    );
}

#[test]
fn uds_oversize_prefix_poisons_decoder_before_body_is_accepted() {
    let limit = 16;
    let announced = 17_u32.to_be_bytes();
    let mut decoder = uds_codec::Decoder::new(limit);

    assert!(
        decoder
            .push(&announced[..2])
            .expect("partial prefix")
            .is_empty()
    );
    let error = decoder
        .push(&announced[2..])
        .expect_err("oversize must fail");
    assert!(matches!(
        error,
        CodecError::FrameLimitExceeded {
            frame_limit: 16,
            announced_len: Some(17)
        }
    ));
    assert!(decoder.is_poisoned());

    let valid = uds_codec::encode(&transcript()[0], TEST_FRAME_LIMIT).expect("valid frame");
    assert!(matches!(
        decoder.push(&valid),
        Err(CodecError::DecoderPoisoned)
    ));
}

#[test]
fn uds_empty_frame_is_rejected_and_poisoned() {
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
    let error = decoder
        .push(&0_u32.to_be_bytes())
        .expect_err("empty frame must fail");
    assert!(matches!(error, CodecError::EmptyFrame));
    assert!(decoder.is_poisoned());
}

#[test]
fn uds_accepts_a_frame_exactly_at_the_limit() {
    let frame = &transcript()[0];
    let ws = ws_codec::encode(frame, TEST_FRAME_LIMIT).expect("measure frame");
    let exact_limit = ws.len();
    let uds = uds_codec::encode(frame, exact_limit).expect("max-exact encode");
    let mut decoder = uds_codec::Decoder::new(exact_limit);

    assert_eq!(
        decoder.push(&uds).expect("max-exact decode"),
        vec![frame.clone()]
    );
    assert!(matches!(
        uds_codec::encode(frame, exact_limit - 1),
        Err(CodecError::FrameLimitExceeded {
            frame_limit,
            announced_len: None
        }) if frame_limit == exact_limit - 1
    ));
}

#[test]
fn uds_valid_utf8_but_invalid_json_body_poisons_decoder() {
    let body = b"{\"v\":1,";
    let length_prefix = u32::try_from(body.len())
        .expect("prefix fits")
        .to_be_bytes();
    let mut framed = length_prefix.to_vec();
    framed.extend_from_slice(body);
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);

    assert!(matches!(decoder.push(&framed), Err(CodecError::Json(_))));
    assert!(decoder.is_poisoned());
}

#[test]
fn ws_empty_message_is_rejected_as_an_empty_frame() {
    assert!(matches!(
        ws_codec::decode("", TEST_FRAME_LIMIT),
        Err(CodecError::EmptyFrame)
    ));
}

#[test]
fn uds_invalid_utf8_body_poisons_decoder() {
    let bytes = [0, 0, 0, 1, 0xff];
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);

    assert!(matches!(
        decoder.push(&bytes),
        Err(CodecError::InvalidUtf8(_))
    ));
    assert!(decoder.is_poisoned());
}

#[test]
fn ws_limit_is_checked_before_json_decode() {
    let error = ws_codec::decode("not-json", 3).expect_err("limit wins");
    assert!(matches!(
        error,
        CodecError::FrameLimitExceeded {
            frame_limit: 3,
            announced_len: Some(8)
        }
    ));
}
