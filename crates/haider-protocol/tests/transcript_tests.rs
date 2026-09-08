#![allow(clippy::expect_used)]
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::SessionId;
use haider_protocol::transcript::{SessionTranscriptPage, SessionTranscriptRequest};
use serde_json::{Value, json};

fn event(seq: u64, payload: Value) -> RawEnvelope {
    serde_json::from_value(json!({"schema_version":1,"event_id":format!("e-{seq}"),"seq":seq,
        "session_id":"source","device_id":"test","worker_generation":1,"authority_epoch":0,
        "committed_at_ms":seq,"render":{"ui":true,"durable":true,"prompt":"verbatim"},"payload":payload})).expect("envelope")
}
fn request(after_seq: u64, limit: u32) -> SessionTranscriptRequest {
    SessionTranscriptRequest {
        session_id: SessionId::new("source"),
        after_seq,
        limit,
    }
}

#[test]
fn transcript_pages_advance_over_omitted_events_and_keep_split_tool_results() {
    let events = vec![
        event(1, json!({"type":"future_fact"})),
        event(
            2,
            json!({"type":"user_message","text":"handoff [REDACTED]","attachments":[]}),
        ),
        event(
            3,
            json!({"type":"item","event":"delta","item_id":"a","delta":{"delta":"text","text":"advisory"}}),
        ),
        event(
            4,
            json!({"type":"item","event":"completed","item_id":"a","item":{"item":"agent_message","text":"final answer"}}),
        ),
        event(
            5,
            json!({"type":"item","event":"completed","item_id":"t","item":{"item":"tool_call","call_id":"call-1","name":"fs_read","args":{"path":"[REDACTED]"},"status":"completed"}}),
        ),
        event(
            6,
            json!({"type":"tool_result","call_id":"call-1","result":{"preview":"result [REDACTED]","truncated":false,"effects":[],"status":"completed"}}),
        ),
        event(7, json!({"type":"run_state","state":"done"})),
    ];
    let whole = SessionTranscriptPage::project(&request(0, 100), 7, &events);
    let mut rows = Vec::new();
    let mut cursor = 0;
    while cursor < 7 {
        let page = SessionTranscriptPage::project(
            &request(cursor, 1),
            7,
            &events[cursor as usize..cursor as usize + 1],
        );
        assert_eq!(page.next_after_seq, cursor + 1);
        assert_eq!(page.has_more, cursor + 1 < 7);
        rows.extend(page.rows);
        cursor = page.next_after_seq;
    }
    assert_eq!(rows, whole.rows);
    let text = whole.render_text();
    for expected in [
        "[2] user: handoff [REDACTED]",
        "[4] assistant: final answer",
        "[5] tool call call-1",
        "[6] tool result call-1",
        "result [REDACTED]",
        "[7] terminal: Done",
    ] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }
    assert!(!text.contains("advisory"));
    assert!(whole.untrusted);
    let empty = SessionTranscriptPage::project(&request(20, 1), 7, &[]);
    assert_eq!(empty.next_after_seq, 20);
    assert!(!empty.has_more);
}

#[test]
fn transcript_output_is_bounded_with_a_resumable_cursor_and_unicode() {
    let events: Vec<_> = (1..=100)
        .map(|seq| {
            event(
                seq,
                json!({"type":"user_message","text":"界".repeat(9000),"attachments":[]}),
            )
        })
        .collect();
    let page = SessionTranscriptPage::project(&request(0, 100), 100, &events);
    assert!(page.rows.iter().all(|row| row.truncated));
    assert!(page.rows.len() < 100);
    assert!(page.render_text().len() < 70_000);
    assert!(page.has_more);
    let next = SessionTranscriptPage::project(
        &request(page.next_after_seq, 100),
        100,
        &events[page.next_after_seq as usize..],
    );
    assert_eq!(next.rows[0].seq, page.next_after_seq + 1);
}

#[test]
fn transcript_omits_private_state_and_does_not_open_artifacts() {
    let events = [
        event(
            1,
            json!({"type":"provider_opaque","secret":"must-not-appear"}),
        ),
        event(
            2,
            json!({"type":"item","event":"completed","item_id":"reasoning","item":{"item":"reasoning","summary":"must-not-appear"}}),
        ),
        event(
            3,
            json!({"type":"item","event":"completed","item_id":"ext","item":{"item":"extension","kind":"unknown","data":{"secret":"must-not-appear"}}}),
        ),
    ];
    let page = SessionTranscriptPage::project(&request(0, 100), 3, &events);
    assert!(page.rows.is_empty());
    assert_eq!(page.next_after_seq, 3);
}
