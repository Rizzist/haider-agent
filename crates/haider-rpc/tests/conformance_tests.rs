//! Shared WS/UDS codec conformance and UDS stream framing cases.
#![allow(clippy::expect_used)]

mod common;

use common::{TEST_FRAME_LIMIT, golden_descriptor, transcript};
use haider_rpc::{
    CodecError, CommandId, FEATURE_ACCOUNT_OAUTH_IMPORT_V1, RequestBody, RequestId, ResponseBody,
    WireFrame, uds_codec, ws_codec,
};

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
    let batch = decoder.push(bytes);
    assert!(batch.error.is_none(), "UDS decode: {:?}", batch.error);
    assert_eq!(batch.frames.len(), 1);
    batch.frames.into_iter().next().expect("one frame")
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

/// MUTATION CHECK: change the import feature literal or either
/// `account.oauth_import` serde rename. Expected runtime failure: the literal
/// or encoded-method assertion no longer matches the served wire contract.
#[test]
fn oauth_import_bodies_round_trip_and_feature_is_pinned() {
    assert_eq!(FEATURE_ACCOUNT_OAUTH_IMPORT_V1, "account_oauth_import_v1");
    let frames = [
        WireFrame::Request {
            request_id: RequestId::new("request-oauth-import"),
            body: RequestBody::AccountOAuthImport {
                command_id: CommandId::new("command-oauth-import"),
                source: "codex".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-oauth-import"),
            body: ResponseBody::AccountOAuthImport {
                descriptor: golden_descriptor(),
                revision: 17,
            },
        },
    ];
    for frame in frames {
        let ws = ws_codec::encode(&frame, TEST_FRAME_LIMIT).expect("WS encode");
        assert!(ws.contains(r#""method":"account.oauth_import""#));
        assert_eq!(
            ws_codec::decode(&ws, TEST_FRAME_LIMIT).expect("WS decode"),
            frame
        );
        let uds = uds_codec::encode(&frame, TEST_FRAME_LIMIT).expect("UDS encode");
        assert_eq!(uds_decode(&uds), frame);
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
        let batch = decoder.push(&[byte]);
        assert!(batch.error.is_none(), "drip decode: {:?}", batch.error);
        decoded.extend(batch.frames);
    }

    assert_eq!(decoded, expected);
}

#[test]
fn uds_coalesced_frames_are_all_yielded() {
    let frames = transcript();
    let mut chunk = uds_codec::encode(&frames[0], TEST_FRAME_LIMIT).expect("first");
    chunk.extend(uds_codec::encode(&frames[1], TEST_FRAME_LIMIT).expect("second"));

    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
    let batch = decoder.push(&chunk);
    assert!(batch.error.is_none(), "coalesced decode: {:?}", batch.error);
    assert_eq!(batch.frames, frames[..2]);
}

#[test]
fn uds_delivery_and_terminal_error_are_invariant_at_every_split_point() {
    let limit = TEST_FRAME_LIMIT;
    let expected = transcript()[0].clone();
    let mut bytes = uds_codec::encode(&expected, limit).expect("valid frame");
    bytes.extend_from_slice(
        &u32::try_from(limit + 1)
            .expect("test limit fits prefix")
            .to_be_bytes(),
    );

    let mut one_chunk_decoder = uds_codec::Decoder::new(limit);
    let one_chunk = one_chunk_decoder.push(&bytes);
    assert_eq!(one_chunk.frames, vec![expected.clone()]);
    assert!(matches!(
        one_chunk.error,
        Some(CodecError::FrameLimitExceeded {
            frame_limit,
            announced_len: Some(announced_len)
        }) if frame_limit == limit && announced_len == limit + 1
    ));

    // Every split includes the same transcript; split == 0 feeds all nonempty
    // bytes in one push after an intentionally empty push.
    for split in 0..=bytes.len() {
        let mut decoder = uds_codec::Decoder::new(limit);
        let first = decoder.push(&bytes[..split]);
        let mut decoded = first.frames;
        let mut terminal = first.error;

        if terminal.is_none() {
            let second = decoder.push(&bytes[split..]);
            decoded.extend(second.frames);
            terminal = second.error;
        }

        assert_eq!(decoded, vec![expected.clone()], "split {split}");
        assert!(
            matches!(
                terminal,
                Some(CodecError::FrameLimitExceeded {
                    frame_limit,
                    announced_len: Some(announced_len)
                }) if frame_limit == limit && announced_len == limit + 1
            ),
            "split {split}: {terminal:?}"
        );
        assert!(decoder.is_poisoned(), "split {split}");

        let after_error = decoder.push(&[]);
        assert!(after_error.frames.is_empty(), "split {split}");
        assert!(
            matches!(after_error.error, Some(CodecError::DecoderPoisoned)),
            "split {split}"
        );
    }
}

#[test]
fn uds_split_length_prefix_is_buffered_without_body_allocation() {
    let frame = &transcript()[0];
    let bytes = uds_codec::encode(frame, TEST_FRAME_LIMIT).expect("encode");
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);

    let first = decoder.push(&bytes[..2]);
    assert!(first.frames.is_empty());
    assert!(first.error.is_none());
    let second = decoder.push(&bytes[2..]);
    assert!(second.error.is_none(), "rest of frame: {:?}", second.error);
    assert_eq!(second.frames, vec![frame.clone()]);
}

#[test]
fn uds_oversize_prefix_poisons_decoder_before_body_is_accepted() {
    let limit = 16;
    let announced = 17_u32.to_be_bytes();
    let mut decoder = uds_codec::Decoder::new(limit);

    let partial = decoder.push(&announced[..2]);
    assert!(partial.frames.is_empty());
    assert!(partial.error.is_none());
    let rejected = decoder.push(&announced[2..]);
    assert!(rejected.frames.is_empty());
    let error = rejected.error.expect("oversize must fail");
    assert!(matches!(
        error,
        CodecError::FrameLimitExceeded {
            frame_limit: 16,
            announced_len: Some(17)
        }
    ));
    assert!(decoder.is_poisoned());

    let valid = uds_codec::encode(&transcript()[0], TEST_FRAME_LIMIT).expect("valid frame");
    let poisoned = decoder.push(&valid);
    assert!(poisoned.frames.is_empty());
    assert!(matches!(poisoned.error, Some(CodecError::DecoderPoisoned)));
}

#[test]
fn uds_empty_frame_is_rejected_and_poisoned() {
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
    let rejected = decoder.push(&0_u32.to_be_bytes());
    assert!(rejected.frames.is_empty());
    let error = rejected.error.expect("empty frame must fail");
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

    let batch = decoder.push(&uds);
    assert!(batch.error.is_none(), "max-exact decode: {:?}", batch.error);
    assert_eq!(batch.frames, vec![frame.clone()]);
    assert!(matches!(
        uds_codec::encode(frame, exact_limit - 1),
        Err(CodecError::FrameLimitExceeded {
            frame_limit,
            announced_len: None
        }) if frame_limit == exact_limit - 1
    ));
}

#[test]
fn bounded_encoder_capacity_does_not_exceed_an_exact_frame_limit() {
    let frame = &transcript()[0];
    let measured = ws_codec::encode(frame, TEST_FRAME_LIMIT).expect("measure frame");
    let exact_limit = measured.len();
    let encoded = ws_codec::encode(frame, exact_limit).expect("encode at exact limit");

    assert_eq!(encoded.len(), exact_limit);
    assert!(
        encoded.capacity() <= exact_limit,
        "capacity {} exceeded frame limit {exact_limit}",
        encoded.capacity()
    );
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

    let rejected = decoder.push(&framed);
    assert!(rejected.frames.is_empty());
    assert!(matches!(rejected.error, Some(CodecError::Json(_))));
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

    let rejected = decoder.push(&bytes);
    assert!(rejected.frames.is_empty());
    assert!(matches!(rejected.error, Some(CodecError::InvalidUtf8(_))));
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
