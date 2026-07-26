#![allow(clippy::expect_used)]

use haider_protocol::provider::{Block, FinishReason, StreamEvent, Usage, UsageSource};
use haider_provider::{
    FakeProvider, FakeStep, Message, MessageRole, Provider, ProviderErrorKind, TurnRequest,
};

fn request() -> TurnRequest {
    TurnRequest {
        messages: vec![Message {
            role: MessageRole::User,
            blocks: vec![Block::Text {
                text: "hello".into(),
            }],
        }],
        model: "fixture-model".into(),
        max_tokens: 128,
    }
}

#[tokio::test]
async fn json_fixture_drives_events_and_records_request() {
    let script = r#"[
        {"step":"emit_text","text":"hello"},
        {"step":"emit_usage","usage":{
            "input":2,"output":1,"reasoning":0,"cached":0,"source":"locally_exact"
        }},
        {"step":"finish","reason":"end_turn"}
    ]"#;
    let provider = FakeProvider::from_json(script).expect("fixture parses");
    let mut stream = provider
        .stream_turn(request())
        .await
        .expect("stream starts");

    assert_eq!(
        stream
            .recv()
            .await
            .expect("text event")
            .expect("valid event"),
        StreamEvent::TextDelta {
            text: "hello".into()
        }
    );
    assert_eq!(
        stream
            .recv()
            .await
            .expect("usage event")
            .expect("valid event"),
        StreamEvent::UsageUpdate(Usage {
            input: 2,
            output: 1,
            reasoning: 0,
            cached: 0,
            source: UsageSource::LocallyExact,
            account: None,
        })
    );
    assert_eq!(
        stream
            .recv()
            .await
            .expect("finish event")
            .expect("valid event"),
        StreamEvent::Finish {
            reason: FinishReason::EndTurn
        }
    );
    assert_eq!(provider.requests(), vec![request()]);
}

#[tokio::test]
async fn split_utf8_is_reassembled_without_replacement_characters() {
    let provider = FakeProvider::new(vec![
        FakeStep::SplitUtf8 {
            text: "🌍".into()
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut stream = provider
        .stream_turn(request())
        .await
        .expect("stream starts");
    let event = stream
        .recv()
        .await
        .expect("text event")
        .expect("valid utf8");
    assert_eq!(
        event,
        StreamEvent::TextDelta {
            text: "🌍".into()
        }
    );
}

#[tokio::test]
async fn malformed_frame_is_a_typed_stream_error() {
    let provider = FakeProvider::new(vec![FakeStep::MalformedFrame]);
    let mut stream = provider
        .stream_turn(request())
        .await
        .expect("stream starts");
    let error = stream
        .recv()
        .await
        .expect("error item")
        .expect_err("malformed frame fails");
    assert_eq!(error.kind, ProviderErrorKind::MalformedFrame);
}

#[tokio::test]
async fn fake_capabilities_are_explicit() {
    let capabilities = FakeProvider::new(Vec::new()).capabilities().await;
    assert_eq!(capabilities.provider, "fake");
    assert_eq!(capabilities.context_limit, 1_000_000);
}

#[tokio::test]
async fn script_segments_are_consumed_one_request_at_a_time() {
    let provider = FakeProvider::new(vec![
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "call-1".into(),
        },
        FakeStep::EmitText {
            text: "continued".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut first = provider
        .stream_turn(request())
        .await
        .expect("first request starts");
    assert_eq!(
        first.recv().await.expect("finish").expect("valid event"),
        StreamEvent::Finish {
            reason: FinishReason::ToolUse
        }
    );
    let mut next = request();
    next.messages
        .push(Message::tool_result("call-1", r#"{"ok":true}"#, false));
    let mut second = provider
        .stream_turn(next)
        .await
        .expect("second request observes result");
    assert_eq!(
        second.recv().await.expect("text").expect("valid event"),
        StreamEvent::TextDelta {
            text: "continued".into()
        }
    );
}
