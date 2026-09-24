#![allow(clippy::expect_used)]

use haider_protocol::provider::{FinishReason, StreamEvent};
use serde_json::{Value, json};

use super::SseDecoder;
use crate::{ProviderErrorKind, ProviderStreamItem};

fn start() -> Value {
    json!({"type":"message_start","message":{"content":[],"usage":{"input_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":3}}})
}

fn block(index: usize, content: Value) -> Value {
    json!({"type":"content_block_start","index":index,"content_block":content})
}

fn delta(index: usize, delta: Value) -> Value {
    json!({"type":"content_block_delta","index":index,"delta":delta})
}

fn stop(index: usize) -> Value {
    json!({"type":"content_block_stop","index":index})
}

fn message_delta(reason: &str) -> Value {
    json!({"type":"message_delta","delta":{"stop_reason":reason}})
}

fn wire(frames: &[Value]) -> Vec<u8> {
    frames
        .iter()
        .map(|frame| {
            format!(
                "event: {}\ndata: {frame}\n\n",
                frame["type"].as_str().expect("type")
            )
        })
        .collect::<String>()
        .into_bytes()
}

fn replay(frames: &[Value]) -> Vec<ProviderStreamItem> {
    replay_bytes(&wire(frames), 1, false)
}

fn replay_bytes(bytes: &[u8], chunk_size: usize, native_computer: bool) -> Vec<ProviderStreamItem> {
    let mut decoder = SseDecoder::with_native_computer(None, native_computer);
    let mut events = Vec::new();
    for chunk in bytes.chunks(chunk_size) {
        events.extend(decoder.push(chunk));
    }
    events.extend(decoder.finish());
    events
}

fn finish(reason: FinishReason) -> ProviderStreamItem {
    Ok(StreamEvent::Finish { reason })
}

// Reconstructed anomalous ordering, not captured raw SSE. The production
// journal retained normalized tool fragments and the message_delta error;
// it did not retain the wire indices, stop_reason, or usage from that frame.
#[test]
fn message_delta_closes_text_and_preserves_cumulative_usage() {
    for delayed_stop in [false, true] {
        let mut frames = vec![
            start(),
            block(0, json!({"type":"text","text":"Hello "})),
            delta(0, json!({"type":"text_delta","text":"🌍"})),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        ];
        if delayed_stop {
            frames.push(stop(0));
        }
        frames.extend([
            json!({"type":"message_delta","delta":{},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ]);
        let events = replay(&frames);
        assert_eq!(events.len(), 5);
        assert_eq!(
            events[0],
            Ok(StreamEvent::TextDelta {
                text: "Hello ".into()
            })
        );
        assert_eq!(
            events[1],
            Ok(StreamEvent::TextDelta {
                text: "🌍".into()
            })
        );
        for (event, output) in events[2..4].iter().zip([4, 5]) {
            let Ok(StreamEvent::UsageUpdate(usage)) = event else {
                panic!("usage: {event:?}")
            };
            assert_eq!((usage.input, usage.output, usage.cached), (7, output, 3));
        }
        assert_eq!(events[4], finish(FinishReason::EndTurn));
    }
}

#[test]
fn message_delta_closes_signed_thinking_without_losing_replay_or_signature() {
    let events = replay(&[
        start(),
        block(0, json!({"type":"thinking","thinking":"seed "})),
        delta(0, json!({"type":"thinking_delta","thinking":"thought"})),
        delta(0, json!({"type":"signature_delta","signature":"sig-"})),
        delta(0, json!({"type":"signature_delta","signature":"tail"})),
        message_delta("end_turn"),
        stop(0),
        json!({"type":"message_stop"}),
    ]);
    assert_eq!(
        events,
        vec![
            Ok(StreamEvent::ReasoningDelta {
                text: "thought".into()
            }),
            Ok(StreamEvent::ProviderOpaque {
                provider: "anthropic".into(),
                data: json!({"type":"thinking","thinking":"seed thought","signature":"sig-tail"})
                    .into(),
            }),
            finish(FinishReason::EndTurn),
        ]
    );
}

#[test]
fn message_delta_closes_unsigned_thinking_without_fabricating_a_signature() {
    assert_eq!(
        replay(&[
            start(),
            block(0, json!({"type":"thinking","thinking":""})),
            delta(0, json!({"type":"thinking_delta","thinking":"partial"})),
            message_delta("max_tokens"),
            json!({"type":"message_stop"}),
        ]),
        vec![
            Ok(StreamEvent::ReasoningDelta {
                text: "partial".into()
            }),
            finish(FinishReason::MaxTokens)
        ]
    );
}

#[test]
fn message_delta_closes_tool_use_once_with_all_argument_fragments() {
    for delayed_stop in [false, true] {
        let mut frames = vec![
            start(),
            block(
                0,
                json!({"type":"tool_use","id":"call","name":"fs_read","input":{}}),
            ),
            delta(
                0,
                json!({"type":"input_json_delta","partial_json":"{\"path\":"}),
            ),
            delta(
                0,
                json!({"type":"input_json_delta","partial_json":"\"sample\"}"}),
            ),
            message_delta("tool_use"),
        ];
        if delayed_stop {
            frames.push(stop(0));
        }
        frames.extend([message_delta("tool_use"), json!({"type":"message_stop"})]);
        assert_eq!(
            replay(&frames),
            vec![
                Ok(StreamEvent::ToolCallStart {
                    call_id: "call".into(),
                    name: "fs_read".into()
                }),
                Ok(StreamEvent::ToolCallArgsDelta {
                    call_id: "call".into(),
                    args_fragment: "{\"path\":".into()
                }),
                Ok(StreamEvent::ToolCallArgsDelta {
                    call_id: "call".into(),
                    args_fragment: "\"sample\"}".into()
                }),
                Ok(StreamEvent::ToolCallEnd {
                    call_id: "call".into()
                }),
                finish(FinishReason::ToolUse),
            ]
        );
    }
}

#[test]
fn message_delta_preserves_truncated_tool_arguments_for_actor_validation() {
    // Shape of the failing request's tail: a completed tool followed by a
    // second tool with five fragments, then message_delta before its stop.
    // IDs/content are synthetic; max_tokens is a hypothesis, not a captured
    // stop_reason. The decoder must not invent a closing brace or tool input.
    let fragments = ["", "{\"path\"", ": \"exam", "ple_fil", "e.py\""];
    let mut frames = vec![
        start(),
        block(
            0,
            json!({"type":"tool_use","id":"first","name":"fs_read","input":{}}),
        ),
        delta(0, json!({"type":"input_json_delta","partial_json":"{}"})),
        stop(0),
        block(
            1,
            json!({"type":"tool_use","id":"second","name":"fs_write","input":{}}),
        ),
    ];
    frames.extend(fragments.iter().map(|fragment| {
        delta(
            1,
            json!({"type":"input_json_delta","partial_json":fragment}),
        )
    }));
    frames.extend([message_delta("max_tokens"), json!({"type":"message_stop"})]);
    let events = replay(&frames);
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let args: String = events
        .iter()
        .filter_map(|event| match event {
            Ok(StreamEvent::ToolCallArgsDelta {
                call_id,
                args_fragment,
            }) if call_id == "second" => Some(args_fragment.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(args, fragments.concat());
    assert!(serde_json::from_str::<Value>(&args).is_err());
    // The stopped first call has complete valid JSON and remains executable.
    // The second call is open and must not acquire an End.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(StreamEvent::ToolCallEnd { call_id }) if call_id == "first"))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(event,
        Ok(StreamEvent::ToolCallEnd { call_id }) if call_id == "second"
    )));
    assert_eq!(events.last(), Some(&finish(FinishReason::MaxTokens)));
}

#[test]
fn message_delta_finalizes_all_open_blocks_in_index_order() {
    let events = replay(&[
        start(),
        block(2, json!({"type":"redacted_thinking","data":"synthetic"})),
        block(
            0,
            json!({"type":"server_tool_use","id":"server","name":"web_search","input":{}}),
        ),
        delta(
            0,
            json!({"type":"input_json_delta","partial_json":"{\"query\":\"sample\"}"}),
        ),
        block(
            1,
            json!({"type":"web_search_tool_result","tool_use_id":"server","content":[]}),
        ),
        message_delta("end_turn"),
        stop(0),
        stop(1),
        stop(2),
        json!({"type":"message_stop"}),
    ]);
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert_eq!(events.len(), 6);
    assert!(
        matches!(&events[0], Ok(StreamEvent::ProviderOpaque { data, .. }) if data.template()["type"] == "server_tool_use")
    );
    assert!(
        matches!(&events[1], Ok(StreamEvent::ServerToolUse { args, .. }) if args == &json!({"query":"sample"}))
    );
    assert!(
        matches!(&events[2], Ok(StreamEvent::ProviderOpaque { data, .. }) if data.template()["type"] == "web_search_tool_result")
    );
    assert!(matches!(
        &events[3],
        Ok(StreamEvent::ServerToolResult {
            is_error: false,
            ..
        })
    ));
    assert_eq!(
        events[4],
        Ok(StreamEvent::ProviderOpaque {
            provider: "anthropic".into(),
            data: json!({"type":"redacted_thinking","data":"synthetic"}).into()
        })
    );
    assert_eq!(events[5], finish(FinishReason::EndTurn));
}

#[test]
fn message_delta_implicit_stop_matches_existing_wire_goldens_at_chunk_boundaries() {
    for fixture in [
        include_bytes!("../../tests/fixtures/anthropic/text_only.sse").as_slice(),
        include_bytes!("../../tests/fixtures/anthropic/tool_call.sse").as_slice(),
        include_bytes!("../../tests/fixtures/anthropic/usage_heavy.sse").as_slice(),
    ] {
        let original = std::str::from_utf8(fixture).expect("fixture UTF-8");
        let mut frames = original.split("\n\n").collect::<Vec<_>>();
        let last_stop = frames
            .iter()
            .rposition(|frame| frame.starts_with("event: content_block_stop"))
            .expect("block stop");
        frames.remove(last_stop);
        let implicit = frames.join("\n\n");
        let expected = replay_bytes(fixture, fixture.len(), false);
        assert!(matches!(
            expected.last(),
            Some(Ok(StreamEvent::Finish { .. }))
        ));
        for size in [1, 2, 7, 31, fixture.len()] {
            assert_eq!(replay_bytes(fixture, size, false), expected);
            assert_eq!(replay_bytes(implicit.as_bytes(), size, false), expected);
        }
    }
}

#[test]
fn message_delta_rejects_late_content_and_unmatched_or_duplicate_stops() {
    for tail in [
        vec![stop(1)],
        vec![stop(0), stop(0)],
        vec![delta(0, json!({"type":"text_delta","text":"late"}))],
        vec![block(1, json!({"type":"text","text":"late"}))],
        vec![message_delta("tool_use")],
    ] {
        let mut frames = vec![
            start(),
            block(0, json!({"type":"text","text":""})),
            message_delta("end_turn"),
        ];
        frames.extend(tail);
        frames.push(json!({"type":"message_stop"}));
        let events = replay(&frames);
        assert_eq!(
            events
                .last()
                .expect("error")
                .as_ref()
                .expect_err("malformed")
                .kind,
            ProviderErrorKind::MalformedFrame
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(StreamEvent::Finish { .. })))
        );
    }
}

#[test]
fn message_delta_keeps_unparseable_frames_and_unterminated_streams_fail_closed() {
    for tail in [
        vec![message_delta("unknown_stop_reason")],
        vec![json!({"type":"message_delta","delta":{},"usage":{"output_tokens":"bad"}})],
        vec![json!({"type":"message_stop"})],
        vec![
            json!({"type":"message_delta","delta":{}}),
            json!({"type":"message_stop"}),
        ],
    ] {
        let mut frames = vec![start(), block(0, json!({"type":"text","text":""}))];
        frames.extend(tail);
        let events = replay(&frames);
        assert_eq!(
            events
                .last()
                .expect("error")
                .as_ref()
                .expect_err("malformed")
                .kind,
            ProviderErrorKind::MalformedFrame
        );
    }
    let events = replay(&[
        start(),
        block(0, json!({"type":"text","text":""})),
        message_delta("end_turn"),
    ]);
    assert_eq!(
        events
            .last()
            .expect("error")
            .as_ref()
            .expect_err("EOF")
            .kind,
        ProviderErrorKind::StreamInterrupted
    );
}

#[test]
fn message_delta_keeps_native_computer_and_server_tool_json_validation() {
    for native_computer in [false, true] {
        let tool_type = if native_computer {
            "tool_use"
        } else {
            "server_tool_use"
        };
        let bytes = wire(&[
            start(),
            block(
                0,
                json!({"type":tool_type,"id":"call","name":"computer","input":{}}),
            ),
            delta(0, json!({"type":"input_json_delta","partial_json":"{"})),
            message_delta("tool_use"),
            json!({"type":"message_stop"}),
        ]);
        let events = replay_bytes(&bytes, 1, native_computer);
        assert_eq!(
            events
                .last()
                .expect("error")
                .as_ref()
                .expect_err("invalid JSON")
                .kind,
            ProviderErrorKind::MalformedFrame
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(StreamEvent::ToolCallEnd { .. })))
        );
    }
}

#[test]
fn message_delta_preserves_citation_text_and_sources() {
    let citation = json!({"type":"web_search_result_location","url":"https://example.com/source","title":"Source","encrypted_index":"synthetic"});
    let events = replay(&[
        start(),
        block(0, json!({"type":"text","text":"A "})),
        delta(0, json!({"type":"text_delta","text":"claim"})),
        delta(0, json!({"type":"citations_delta","citation":citation})),
        message_delta("end_turn"),
        json!({"type":"message_stop"}),
    ]);
    assert_eq!(
        events,
        vec![
            Ok(StreamEvent::TextDelta { text: "A ".into() }),
            Ok(StreamEvent::TextDelta {
                text: "claim".into()
            }),
            Ok(StreamEvent::ProviderOpaque {
                provider: "anthropic".into(),
                data: json!({"type":"text","text":"A claim","citations":[citation]}).into()
            }),
            Ok(StreamEvent::WebSources {
                sources: vec![haider_protocol::provider::WebSource {
                    url: "https://example.com/source".into(),
                    title: Some("Source".into())
                }]
            }),
            finish(FinishReason::EndTurn),
        ]
    );
}

#[test]
fn message_delta_normalizes_native_computer_input_before_ending_tool() {
    let frames = [
        start(),
        block(
            0,
            json!({"type":"tool_use","id":"call","name":"computer","input":{}}),
        ),
        delta(
            0,
            json!({"type":"input_json_delta","partial_json":"{\"action\":\"screenshot\"}"}),
        ),
        message_delta("tool_use"),
        json!({"type":"message_stop"}),
    ];
    let implicit = replay_bytes(&wire(&frames), 1, true);
    let mut explicit_frames = frames.to_vec();
    explicit_frames.insert(3, stop(0));
    let explicit = replay_bytes(&wire(&explicit_frames), 1, true);
    assert_eq!(implicit, explicit);
    assert_eq!(implicit.len(), 4);
    assert!(matches!(
        &implicit[1],
        Ok(StreamEvent::ToolCallArgsDelta { .. })
    ));
    assert_eq!(
        implicit[2],
        Ok(StreamEvent::ToolCallEnd {
            call_id: "call".into()
        })
    );
    assert_eq!(implicit[3], finish(FinishReason::ToolUse));
}
