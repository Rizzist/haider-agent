#![allow(clippy::expect_used)]
//! Shared WS/UDS codec conformance and UDS stream framing cases.

mod common;

use common::{TEST_FRAME_LIMIT, golden_descriptor, raw_envelope, transcript};
use haider_rpc::haider_protocol::typed_agent::{
    TypedAgentInstallItem, TypedAgentInstallJob, TypedAgentInstallProgress, TypedAgentInstallState,
    TypedAgentInstallTerminalStateV1, TypedAgentRequiredCli,
};
use haider_rpc::{
    AttachmentId, CodecError, CommandId, FEATURE_ACCOUNT_OAUTH_IMPORT_V1, FEATURE_MONITOR_V1,
    FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1, FEATURE_TYPED_AGENT_INSTALL_CONTROL_V1,
    FEATURE_TYPED_AGENT_INSTALL_V1, RequestBody, RequestId, ResponseBody,
    TypedAgentInstallCancelOutcomeWire, TypedAgentInstallCancelReceiptWire,
    TypedAgentInstallRetryOutcomeWire, TypedAgentInstallRetryReceiptWire,
    TypedAgentInstallRetryRejectionWire, TypedAgentInstallWatchOutcomeWire,
    TypedAgentInstallWatchReceiptWire, TypedAgentInstallWatchRejectionWire, WireEncoding,
    WireFrame, decode_msgpack, encode_msgpack, haider_protocol::ids::SessionId, uds_codec,
    ws_codec,
};

#[test]
fn monitor_feature_is_pinned() {
    assert_eq!(FEATURE_MONITOR_V1, "monitor_v1");
}

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

/// The reconnectable install status is a View-plane read carrying durable,
/// bounded protocol records. Pin both method discriminants and the advertised
/// feature literal so clients never infer progress from live PATH presence.
#[test]
fn typed_agent_install_status_round_trips_and_feature_is_pinned() {
    assert_eq!(FEATURE_TYPED_AGENT_INSTALL_V1, "typed_agent_install_v1");
    let job = TypedAgentInstallJob {
        job_id: "install:reviewer:1".into(),
        agent_type_id: "reviewer".into(),
        agent_type_rev: 1,
        agent_type_digest: "0123456789abcdef0123456789abcdef".into(),
        state: TypedAgentInstallState::Queued,
        cancelled: false,
        progress: TypedAgentInstallProgress {
            total: 1,
            completed: 0,
            current_cli: None,
        },
        error: None,
        created_at_ms: 1_753_500_000_000,
        updated_at_ms: 1_753_500_000_000,
    };
    let item = TypedAgentInstallItem {
        job_id: job.job_id.clone(),
        ordinal: 0,
        required_cli: TypedAgentRequiredCli {
            program: "rg".into(),
        },
        state: TypedAgentInstallState::Queued,
        error: None,
        created_at_ms: job.created_at_ms,
        updated_at_ms: job.updated_at_ms,
    };
    job.validate().expect("bounded install job");
    item.validate().expect("bounded install item");

    let request = RequestBody::LoomInstallStatus {
        job_id: Some(job.job_id.clone()),
        agent_type_id: Some(job.agent_type_id.clone()),
    };
    let response = ResponseBody::LoomInstallStatus {
        jobs: vec![job],
        items: vec![item],
    };
    assert_eq!(
        serde_json::to_value(&request).expect("request JSON"),
        serde_json::json!({
            "method": "loom.install.status",
            "job_id": "install:reviewer:1",
            "agent_type_id": "reviewer",
        })
    );
    assert_eq!(
        serde_json::to_value(&response).expect("response JSON")["method"],
        "loom.install.status"
    );

    let frames = [
        WireFrame::Request {
            request_id: RequestId::new("request-loom-install-status"),
            body: request,
        },
        WireFrame::Response {
            request_id: RequestId::new("request-loom-install-status"),
            body: response,
        },
    ];
    for frame in frames {
        let ws = ws_codec::encode(&frame, TEST_FRAME_LIMIT).expect("WS encode");
        assert!(ws.contains(r#""method":"loom.install.status""#));
        assert_eq!(
            ws_codec::decode(&ws, TEST_FRAME_LIMIT).expect("WS decode"),
            frame
        );
        let uds = uds_codec::encode(&frame, TEST_FRAME_LIMIT).expect("UDS encode");
        assert_eq!(uds_decode(&uds), frame);
    }
}

/// MUTATION CHECK: remove the control feature, either tail-added method, the
/// retry receipt job, or any watch cursor. Expected runtime failure: the exact
/// values or codec round trips below no longer describe a recoverable replay.
#[test]
fn typed_agent_install_control_receipts_and_watch_cursors_round_trip() {
    assert_eq!(
        FEATURE_TYPED_AGENT_INSTALL_CONTROL_V1,
        "typed_agent_install_control_v1"
    );
    let job = TypedAgentInstallJob {
        job_id: "install:reviewer:1".into(),
        agent_type_id: "reviewer".into(),
        agent_type_rev: 1,
        agent_type_digest: "0123456789abcdef0123456789abcdef".into(),
        state: TypedAgentInstallState::Queued,
        cancelled: false,
        progress: TypedAgentInstallProgress {
            total: 1,
            completed: 0,
            current_cli: None,
        },
        error: None,
        created_at_ms: 1_753_500_000_000,
        updated_at_ms: 1_753_500_000_000,
    };
    let event = haider_rpc::haider_protocol::typed_agent::TypedAgentInstallEvent {
        cursor: 41,
        job: job.clone(),
    };
    let frames = [
        WireFrame::Request {
            request_id: RequestId::new("request-install-retry"),
            body: RequestBody::LoomInstallRetry {
                job_id: job.job_id.clone(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-install-retry"),
            body: ResponseBody::LoomInstallRetry {
                receipt: TypedAgentInstallRetryReceiptWire {
                    job_id: job.job_id.clone(),
                    outcome: TypedAgentInstallRetryOutcomeWire::Requeued { job: job.clone() },
                },
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-install-watch"),
            body: RequestBody::LoomInstallWatch {
                job_id: job.job_id.clone(),
                after_cursor: 0,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-install-watch"),
            body: ResponseBody::LoomInstallWatch {
                receipt: TypedAgentInstallWatchReceiptWire {
                    job_id: job.job_id,
                    outcome: TypedAgentInstallWatchOutcomeWire::Watching {
                        requested_after_cursor: 0,
                        replay_through_cursor: 41,
                        next_cursor: 41,
                        events: vec![event],
                    },
                },
            },
        },
    ];
    for frame in frames {
        let ws = ws_codec::encode(&frame, TEST_FRAME_LIMIT).expect("WS encode");
        assert_eq!(
            ws_codec::decode(&ws, TEST_FRAME_LIMIT).expect("WS decode"),
            frame
        );
        let uds = uds_codec::encode(&frame, TEST_FRAME_LIMIT).expect("UDS encode");
        assert_eq!(uds_decode(&uds), frame);
    }

    let retry_rejections = [
        (
            TypedAgentInstallRetryRejectionWire::JobNotFound,
            serde_json::json!({"reason": "job_not_found"}),
        ),
        (
            TypedAgentInstallRetryRejectionWire::StateNotRetryable {
                state: TypedAgentInstallState::Succeeded,
            },
            serde_json::json!({"reason": "state_not_retryable", "state": "succeeded"}),
        ),
        (
            TypedAgentInstallRetryRejectionWire::ContractNotCurrent,
            serde_json::json!({"reason": "contract_not_current"}),
        ),
    ];
    for (rejection, expected) in retry_rejections {
        assert_eq!(
            serde_json::to_value(rejection).expect("retry rejection JSON"),
            expected
        );
    }
    let watch_rejections = [
        (
            TypedAgentInstallWatchRejectionWire::JobNotFound,
            serde_json::json!({"reason": "job_not_found"}),
        ),
        (
            TypedAgentInstallWatchRejectionWire::CursorAhead {
                requested: 42,
                head: 41,
            },
            serde_json::json!({"reason": "cursor_ahead", "requested": 42, "head": 41}),
        ),
    ];
    for (rejection, expected) in watch_rejections {
        assert_eq!(
            serde_json::to_value(rejection).expect("watch rejection JSON"),
            expected
        );
    }

    let retry_rejected = TypedAgentInstallRetryReceiptWire {
        job_id: "missing-install".into(),
        outcome: TypedAgentInstallRetryOutcomeWire::Rejected {
            rejection: TypedAgentInstallRetryRejectionWire::JobNotFound,
        },
    };
    assert_eq!(
        serde_json::to_value(retry_rejected).expect("retry rejected receipt JSON"),
        serde_json::json!({
            "job_id": "missing-install",
            "outcome": {
                "status": "rejected",
                "rejection": {"reason": "job_not_found"}
            }
        })
    );
    let watch_rejected = TypedAgentInstallWatchReceiptWire {
        job_id: "install:reviewer:1".into(),
        outcome: TypedAgentInstallWatchOutcomeWire::Rejected {
            rejection: TypedAgentInstallWatchRejectionWire::CursorAhead {
                requested: 42,
                head: 41,
            },
        },
    };
    assert_eq!(
        serde_json::to_value(watch_rejected).expect("watch rejected receipt JSON"),
        serde_json::json!({
            "job_id": "install:reviewer:1",
            "outcome": {
                "status": "rejected",
                "rejection": {
                    "reason": "cursor_ahead",
                    "requested": 42,
                    "head": 41
                }
            }
        })
    );
}

/// MUTATION CHECK: remove/default/rename `TypedAgentInstallJob.cancelled`,
/// grow the frozen lifecycle enum, or change the cancellation receipt's
/// terminal type. Expected runtime failure: an old job no longer decodes or
/// the distinct cancellation fact below disappears.
#[test]
fn typed_agent_install_cancel_preserves_the_962_state_enum() {
    assert_eq!(
        FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1,
        "typed_agent_install_cancel_v1"
    );
    let old_job = serde_json::json!({
        "job_id": "install:reviewer:1",
        "agent_type_id": "reviewer",
        "agent_type_rev": 1,
        "agent_type_digest": "0123456789abcdef0123456789abcdef",
        "state": "queued",
        "progress": {"total": 1, "completed": 0},
        "created_at_ms": 1,
        "updated_at_ms": 1
    });
    let mut job: TypedAgentInstallJob =
        serde_json::from_value(old_job).expect("v0.0.962 job decodes");
    assert!(!job.cancelled, "omission is the pre-cancel false fact");
    job.state = TypedAgentInstallState::Failed;
    job.cancelled = true;
    job.error = Some("typed-agent installation was cancelled".into());
    job.updated_at_ms = 2;
    job.validate().expect("distinct cancellation carrier");
    let value = serde_json::to_value(&job).expect("cancelled job JSON");
    assert_eq!(value["state"], "failed", "the frozen enum does not grow");
    assert_eq!(value["cancelled"], true, "cancellation remains distinct");
    assert_eq!(
        serde_json::to_value(TypedAgentInstallCancelOutcomeWire::Unknown)
            .expect("unknown-job outcome JSON"),
        serde_json::json!({"status": "unknown"}),
        "an absent requested job uses the contract's exact unknown status"
    );

    let receipt = TypedAgentInstallCancelReceiptWire {
        install_job_id: job.job_id.clone(),
        outcome: TypedAgentInstallCancelOutcomeWire::AlreadyTerminal {
            state: TypedAgentInstallTerminalStateV1::Cancelled,
        },
    };
    let frames = [
        WireFrame::Request {
            request_id: RequestId::new("request-install-cancel"),
            body: RequestBody::LoomInstallCancel {
                install_job_id: job.job_id,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-install-cancel"),
            body: ResponseBody::LoomInstallCancel { receipt },
        },
    ];
    for frame in frames {
        let json = ws_codec::encode(&frame, TEST_FRAME_LIMIT).expect("cancel JSON");
        assert_eq!(
            ws_codec::decode(&json, TEST_FRAME_LIMIT).expect("cancel decode"),
            frame
        );
    }
}

/// MUTATION CHECK: rename or merge any L4 feature token. Expected runtime
/// failure: one negotiated group can no longer be distinguished verbatim.
#[test]
fn loom_l4_feature_tokens_are_exact_and_independent() {
    assert_eq!(
        haider_rpc::FEATURE_LOOM_REGISTRY_CAS_V1,
        "loom_registry_cas_v1"
    );
    assert_eq!(
        haider_rpc::FEATURE_LOOM_REGISTRY_ARCHIVE_V1,
        "loom_registry_archive_v1"
    );
    assert_eq!(haider_rpc::FEATURE_LOOM_VALIDATION_V1, "loom_validation_v1");
    assert_eq!(
        haider_rpc::FEATURE_LOOM_REGISTRY_WATCH_V1,
        "loom_registry_watch_v1"
    );
}

/// MUTATION CHECK: remove the catch-all from the tagged registry record.
/// Expected runtime failure: a future record kind rejects the whole baseline
/// instead of remaining a typed, non-actionable unknown.
#[test]
fn loom_registry_record_kind_is_unknown_tolerant() {
    let record: haider_rpc::haider_protocol::loom::LoomRegistryRecord = serde_json::from_value(
        serde_json::json!({"kind": "future_registry_kind", "record": {"future": true}}),
    )
    .expect("future registry kind decodes");
    assert_eq!(
        record,
        haider_rpc::haider_protocol::loom::LoomRegistryRecord::Unknown
    );
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

#[test]
fn msgpack_round_trips_nested_json_value_on_both_transports() {
    let mut envelope = raw_envelope(42);
    envelope.payload = serde_json::json!({
        "nested": {
            "array": [null, true, -7, 3.5, {"deep": "value"}],
            "unicode": "سلام"
        }
    })
    .into();
    let frame = WireFrame::Event {
        attachment_id: AttachmentId::new("attachment-msgpack"),
        session_id: SessionId::new("session-1"),
        envelope,
    };

    let binary = ws_codec::encode_binary(&frame, TEST_FRAME_LIMIT).expect("MessagePack encode");
    assert_eq!(
        ws_codec::decode_binary(&binary, TEST_FRAME_LIMIT).expect("MessagePack decode"),
        frame
    );

    let uds = uds_codec::encode_with(&frame, TEST_FRAME_LIMIT, WireEncoding::MessagePack)
        .expect("MessagePack UDS encode");
    let mut decoder =
        uds_codec::Decoder::new_with_encoding(TEST_FRAME_LIMIT, WireEncoding::MessagePack);
    let batch = decoder.push(&uds);
    assert!(batch.error.is_none(), "UDS MessagePack: {:?}", batch.error);
    assert_eq!(batch.frames, vec![frame]);
}

#[test]
fn msgpack_empty_and_oversize_decode_refusals_match_json_shapes() {
    assert!(matches!(
        decode_msgpack(&[], TEST_FRAME_LIMIT),
        Err(CodecError::EmptyFrame)
    ));

    let bytes = [0xc0, 0xc0, 0xc0];
    assert!(matches!(
        decode_msgpack(&bytes, 2),
        Err(CodecError::FrameLimitExceeded {
            frame_limit: 2,
            announced_len: Some(3)
        })
    ));
}

#[test]
fn msgpack_encoder_enforces_the_same_bounded_writer_limit_as_json() {
    let frame = transcript()[0].clone();
    let encoded = encode_msgpack(&frame, TEST_FRAME_LIMIT).expect("measure MessagePack frame");
    let exact_limit = encoded.len();

    assert_eq!(
        encode_msgpack(&frame, exact_limit).expect("exact limit"),
        encoded
    );
    assert!(matches!(
        encode_msgpack(&frame, exact_limit - 1),
        Err(CodecError::FrameLimitExceeded {
            frame_limit,
            announced_len: None
        }) if frame_limit == exact_limit - 1
    ));
}

#[test]
fn msgpack_malformed_body_has_a_typed_refusal() {
    assert!(matches!(
        decode_msgpack(&[0xc1], TEST_FRAME_LIMIT),
        Err(CodecError::MessagePackDecode(_))
    ));
}
