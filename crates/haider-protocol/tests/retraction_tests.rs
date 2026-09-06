#![allow(clippy::expect_used)]

use haider_protocol::provider::{FinishReason, StreamEvent};
use haider_protocol::retraction::is_response_delta;

/// Transport/usage progress and empty deltas never close the edit window.
/// MUTATION: treating all non-usage stream events as response content makes
/// Finish, empty text/reasoning/refusal, or an empty source list fail this pin.
#[test]
fn metadata_and_empty_deltas_keep_the_prompt_retractable() {
    let usage = serde_json::from_value(serde_json::json!({
        "input": 12, "output": 0, "source": "locally_exact"
    }))
    .expect("usage metadata");
    for event in [
        StreamEvent::NetworkUnavailable,
        StreamEvent::NetworkRestored,
        StreamEvent::UsageUpdate(usage),
        StreamEvent::Finish {
            reason: FinishReason::EndTurn,
        },
        StreamEvent::TextDelta { text: "".into() },
        StreamEvent::ReasoningDelta { text: "".into() },
        StreamEvent::RefusalDelta { text: "".into() },
        StreamEvent::ToolCallArgsDelta {
            call_id: "call".into(),
            args_fragment: "".into(),
        },
        StreamEvent::WebSources {
            sources: Vec::new(),
        },
    ] {
        assert!(
            !is_response_delta(&event),
            "metadata is not first response content: {event:?}"
        );
    }
}

/// A semantic response can begin with reasoning, refusal, or a tool rather
/// than assistant prose. The retraction fence must cover each such surface.
#[test]
fn nonempty_response_deltas_and_tool_start_close_the_edit_window() {
    for event in [
        StreamEvent::TextDelta {
            text: "answer".into(),
        },
        StreamEvent::TextDelta { text: " ".into() },
        StreamEvent::ReasoningDelta {
            text: "summary".into(),
        },
        StreamEvent::RefusalDelta {
            text: "refused".into(),
        },
        StreamEvent::ToolCallStart {
            call_id: "call".into(),
            name: "read_file".into(),
        },
        StreamEvent::ToolCallArgsDelta {
            call_id: "call".into(),
            args_fragment: "{".into(),
        },
    ] {
        assert!(
            is_response_delta(&event),
            "semantic content closes the editable window: {event:?}"
        );
    }
}
