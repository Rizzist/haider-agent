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
    for payload in [
        json!({"type":"user_message","text":"界".repeat(9000),"attachments":[]}),
        json!({"type":"item","event":"completed","item_id":"report","item":{
            "item":"child_result","report":{"agent":"child","summary":"界".repeat(9000),"verified":"red"}}}),
        json!({"type":"item","event":"completed","item_id":"delivery","item":{
            "item":"extension","kind":"detached_child_result_v1","data":{"agent":"child",
                "task":"界".repeat(9000),"stopped_run_id":"run","state":"cancelled","verified":"red","summary":"report"}}}),
    ] {
        let events: Vec<_> = (1..=100).map(|seq| event(seq, payload.clone())).collect();
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

#[test]
fn transcript_child_reports_and_public_notices_survive_split_pages() {
    let events = [
        event(
            1,
            json!({"type":"item","event":"completed","item_id":"report",
            "item":{"item":"child_result","report":{"agent":"child","summary":"late report 界",
                "verified":"unverified","private_extra":"must-not-appear"}}}),
        ),
        event(
            2,
            json!({"type":"item","event":"completed","item_id":"delivery",
            "item":{"item":"extension","kind":"detached_child_result_v1","data":{
                "agent":"child","task":"review","stopped_run_id":"run","state":"done",
                "verified":"unverified","summary":"late report 界","label":"must-not-appear",
                "private_extra":"must-not-appear"}}}),
        ),
        event(
            3,
            json!({"type":"item","event":"completed","item_id":"stop",
            "item":{"item":"extension","kind":"subagents_still_running_v1","data":{
                "parent_session_id":"source","stopped_run_id":"run","still_running":1,
                "agents":[],"label":"must-not-appear"}}}),
        ),
    ];
    let whole = SessionTranscriptPage::project(&request(0, 100), 3, &events);
    let rows: Vec<_> = events
        .iter()
        .flat_map(|event| {
            SessionTranscriptPage::project(
                &request(event.seq - 1, 1),
                3,
                std::slice::from_ref(event),
            )
            .rows
        })
        .collect();
    assert_eq!(whole.rows, rows);
    assert_eq!(whole.rows.len(), 3);
    for text in [
        whole.render_text(),
        serde_json::to_string(&whole).expect("JSON page"),
    ] {
        assert!(text.contains("late report 界"));
        assert!(text.contains("result delivered"));
        assert!(text.contains("still running"));
        assert!(!text.contains("must-not-appear"));
    }
}

#[test]
fn transcript_delivery_notices_cover_all_terminal_states_and_reasons() {
    for state in ["done", "errored", "cancelled"] {
        for reason in ["main_stopped", "observation_ended"] {
            let events = [event(
                1,
                json!({"type":"item","event":"completed","item_id":"notice",
                "item":{"item":"extension","kind":"detached_child_result_v1","data":{
                    "agent":"child","task":"review","stopped_run_id":"run","state":state,
                    "reason":reason,"verified":"red","summary":"report"}}}),
            )];
            let page = SessionTranscriptPage::project(&request(0, 1), 1, &events);
            assert_eq!(page.rows.len(), 1, "{state}/{reason}");
            assert!(page.render_text().contains("result delivered"));
            assert!(page.render_text().contains(if reason == "main_stopped" {
                "waiting turn was stopped"
            } else {
                "observation window ended"
            }));
        }
    }
}

#[test]
fn transcript_omits_uncompleted_malformed_and_private_child_extensions() {
    let mut events = Vec::new();
    for kind in [
        "unknown",
        "detached_delivery_v1",
        "tool_arguments_finalized_v1",
        "user_command_origin_v1",
        "detached_child_result_v1",
        "subagents_still_running_v1",
    ] {
        events.push(event(
            events.len() as u64 + 1,
            json!({"type":"item","event":"completed","item_id":"private",
            "item":{"item":"extension","kind":kind,"data":{"summary":"must-not-appear"}}}),
        ));
    }
    events.push(event(7, json!({"type":"item","event":"started","item_id":"report",
        "item":{"item":"child_result","report":{"agent":"child","summary":"must-not-appear","verified":"red"}}})));
    let page = SessionTranscriptPage::project(&request(0, 100), 7, &events);
    assert!(page.rows.is_empty());
    assert_eq!(page.next_after_seq, 7);
}
