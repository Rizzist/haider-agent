#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::ids::{AgentId, ItemId};
use crate::item::ToolStatus;
use crate::state::RunState;

fn call(name: &str) -> TurnItem {
    TurnItem::ToolCall {
        call_id: format!("call-{name}"),
        name: name.to_owned(),
        args: serde_json::json!({}),
        status: ToolStatus::InProgress,
    }
}

fn started(id: &str, name: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new(id),
        item: call(name),
    })
}

fn completed(id: &str, name: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(id),
        item: call(name),
    })
}

#[test]
fn tool_identity_maps_every_route_to_its_documented_kind() {
    use ActivityKind as A;
    for (name, kind) in [
        ("computer", A::Computer),
        ("mobile", A::Mobile),
        ("web_fetch", A::Web),
        ("web_search", A::Web),
        ("webextract", A::Web),
        ("browser_navigate", A::Web),
        ("ssh_shell", A::Remote),
        ("fs_write", A::Edit),
        ("fs_edit", A::Edit),
        ("fs_patch", A::Edit),
        ("fs_path", A::Edit),
        ("edit", A::Edit),
        ("process_exec", A::Run),
        ("test_run", A::Run),
        ("tool_script", A::Run),
        ("fs_read", A::Read),
        ("fs_search", A::Read),
        ("fs_glob", A::Read),
        ("session_transcript", A::Read),
        ("spawn_subagent", A::Orchestrate),
        ("message_subagent", A::Orchestrate),
        ("loom_register", A::Orchestrate),
        ("workflow_author", A::Orchestrate),
    ] {
        assert_eq!(A::for_tool(name), Some(kind), "{name}");
    }
    for silent in [
        "todo_write",
        "plan",
        "request_input",
        "task_outcome",
        "monitor",
        "mcp__x__y",
        "",
    ] {
        assert_eq!(A::for_tool(silent), None, "{silent}");
    }
}

#[test]
fn derivation_never_reads_free_text() {
    // A read tool whose ARGUMENTS talk about editing and computers stays read.
    let item = TurnItem::ToolCall {
        call_id: "c".into(),
        name: "fs_read".into(),
        args: serde_json::json!({"path": "computer_edit_ssh_web.md"}),
        status: ToolStatus::Completed,
    };
    assert_eq!(ActivityKind::for_item(&item), Some(ActivityKind::Read));
    // Prose items carry no activity at all.
    let prose = TurnItem::AgentMessage {
        text: "I will now use the computer and ssh".to_owned().into(),
    };
    assert_eq!(ActivityKind::for_item(&prose), None);
    assert_eq!(
        ActivityKind::for_item(&TurnItem::ChildSpawn {
            agent: AgentId::new("a")
        }),
        Some(ActivityKind::Orchestrate)
    );
}

#[test]
fn priority_order_is_the_owner_order() {
    let ranks: Vec<u8> = ActivityKind::ALL.iter().map(|kind| kind.rank()).collect();
    assert_eq!(ranks, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(
        highest([ActivityKind::Read, ActivityKind::Edit, ActivityKind::Run]),
        Some(ActivityKind::Edit)
    );
    assert_eq!(
        highest([ActivityKind::Orchestrate, ActivityKind::Web]),
        Some(ActivityKind::Web)
    );
    assert_eq!(highest([ActivityKind::Unknown]), None);
}

#[test]
fn glyphs_are_two_column_emoji_with_vs16_where_needed() {
    for kind in ActivityKind::ALL {
        let emoji = kind.emoji();
        assert!(!emoji.is_empty());
        let base = emoji.chars().next().unwrap() as u32;
        // Text-default bases must carry VS16.
        if matches!(base, 0x1F5A5 | 0x270F | 0x2699) {
            assert!(emoji.ends_with('\u{FE0F}'), "{kind:?} lacks VS16");
        }
        assert!(kind.text_label().starts_with('['));
        assert!(kind.text_label().is_ascii());
    }
    assert_eq!(ActivityKind::Read.text_label(), "[read]");
    assert_eq!(ActivityKind::Orchestrate.text_label(), "[orch]");
    assert_eq!(ActivityKind::Computer.text_label(), "[screen]");
}

#[test]
fn live_updates_follow_calls_as_they_start() {
    let mut tracker = ActivityTracker::new();
    assert_eq!(tracker.current(0, false), None);
    tracker.observe(&started("1", "fs_read"), 1_000);
    assert_eq!(tracker.current(1_000, false), Some(ActivityKind::Read));
    tracker.observe(&completed("1", "fs_read"), 1_200);
    // Read → edit within the hot window: edit outranks read.
    tracker.observe(&started("2", "fs_edit"), 2_000);
    assert_eq!(tracker.current(2_000, false), Some(ActivityKind::Edit));
    tracker.observe(&completed("2", "fs_edit"), 2_100);
    // Edit → run: edit stays hot and outranks run until the window passes.
    tracker.observe(&started("3", "test_run"), 3_000);
    assert_eq!(tracker.current(3_000, false), Some(ActivityKind::Edit));
    let after_edit_cools = 2_100 + ACTIVITY_HOT_WINDOW_MS + 1;
    assert_eq!(
        tracker.current(after_edit_cools, false),
        Some(ActivityKind::Run),
        "the still-open test run is the current activity"
    );
    // A long-running open call stays current indefinitely.
    assert_eq!(
        tracker.current(3_000 + 20 * 60_000, false),
        Some(ActivityKind::Run)
    );
    assert_eq!(
        tracker.used(),
        vec![ActivityKind::Edit, ActivityKind::Run, ActivityKind::Read]
    );
}

#[test]
fn quiet_falls_back_to_most_recent_then_nothing() {
    let mut tracker = ActivityTracker::new();
    tracker.observe(&started("1", "fs_edit"), 1_000);
    tracker.observe(&completed("1", "fs_edit"), 1_100);
    tracker.observe(&started("2", "fs_read"), 1_500);
    tracker.observe(&completed("2", "fs_read"), 1_600);
    assert_eq!(tracker.current(1_600, false), Some(ActivityKind::Edit));
    // Past the hot window: most recent activity (read), not the priority.
    let quiet = 1_600 + ACTIVITY_HOT_WINDOW_MS + 1;
    assert_eq!(tracker.current(quiet, false), Some(ActivityKind::Read));
    // Past the linger: no icon.
    let silent = 1_600 + ACTIVITY_LINGER_MS + 1;
    assert_eq!(tracker.current(silent, false), None);
    // Live children keep the orchestrating icon while quiet.
    assert_eq!(
        tracker.current(silent, true),
        Some(ActivityKind::Orchestrate)
    );
}

#[test]
fn terminal_runs_show_no_current_activity_but_keep_history() {
    let mut tracker = ActivityTracker::new();
    tracker.observe(&started("1", "computer"), 10);
    tracker.observe(&EventPayload::RunState(RunState::Done), 20);
    assert_eq!(tracker.current(20, true), None);
    assert_eq!(tracker.used(), vec![ActivityKind::Computer]);
    // A later run (message_subagent / monitor wake) revives it.
    let later = 10 + ACTIVITY_HOT_WINDOW_MS + 1;
    tracker.observe(&EventPayload::RunState(RunState::Thinking), later);
    tracker.observe(&started("2", "mobile"), later);
    assert_eq!(tracker.current(later, false), Some(ActivityKind::Mobile));
}

#[test]
fn replay_order_and_duplicate_starts_are_idempotent() {
    let mut once = ActivityTracker::new();
    let mut twice = ActivityTracker::new();
    for tracker in [&mut once, &mut twice] {
        tracker.observe(&started("1", "web_fetch"), 5);
    }
    twice.observe(&started("1", "web_fetch"), 5);
    assert_eq!(once, twice);
    // An out-of-order older fact never rewinds `latest`.
    twice.observe(&completed("0", "fs_read"), 1);
    assert_eq!(
        twice.current(5 + ACTIVITY_HOT_WINDOW_MS + 1, false),
        Some(ActivityKind::Web)
    );
}

#[test]
fn in_flight_is_bounded() {
    let mut tracker = ActivityTracker::new();
    for n in 0..(ACTIVITY_MAX_IN_FLIGHT + 10) {
        tracker.observe(&started(&n.to_string(), "fs_read"), 1);
    }
    assert_eq!(tracker.in_flight.len(), ACTIVITY_MAX_IN_FLIGHT);
}

#[test]
fn wire_names_round_trip_and_unknown_kinds_degrade() {
    for kind in ActivityKind::ALL {
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, format!("\"{}\"", kind.name()));
        assert_eq!(serde_json::from_str::<ActivityKind>(&json).unwrap(), kind);
    }
    assert_eq!(
        serde_json::from_str::<ActivityKind>("\"teleport\"").unwrap(),
        ActivityKind::Unknown
    );
    assert_eq!(ActivityDisplay::Emoji.glyph(ActivityKind::Unknown), None);
}

#[test]
fn display_resolution_precedence() {
    use ActivityDisplay as D;
    assert_eq!(
        D::resolve(None, None, false, Some("xterm-256color")),
        D::Emoji
    );
    assert_eq!(
        D::resolve(None, None, true, Some("xterm-256color")),
        D::Text
    );
    assert_eq!(D::resolve(None, None, false, Some("linux")), D::Text);
    assert_eq!(D::resolve(None, None, false, Some("dumb")), D::Text);
    assert_eq!(D::resolve(None, Some(D::Off), true, None), D::Off);
    assert_eq!(D::resolve(None, Some(D::Emoji), true, None), D::Emoji);
    assert_eq!(
        D::resolve(Some("text"), Some(D::Emoji), false, None),
        D::Text
    );
    assert_eq!(D::resolve(Some("off"), None, false, None), D::Off);
    // An unparseable override is ignored, never a silent disable.
    assert_eq!(D::resolve(Some("sparkles"), None, false, None), D::Emoji);
    assert_eq!(D::Text.glyph(ActivityKind::Run), Some("[run]"));
    assert_eq!(D::Off.glyph(ActivityKind::Run), None);
}

#[test]
fn wire_state_round_trips_and_evaluates_on_the_client_clock() {
    let mut tracker = ActivityTracker::new();
    tracker.observe(&started("1", "fs_read"), 1_000);
    tracker.observe(&completed("1", "fs_read"), 1_100);
    tracker.observe(&started("2", "ssh_shell"), 2_000);
    let state = tracker.state();
    assert_eq!(state.open, vec![ActivityKind::Remote]);
    let json = serde_json::to_value(&state).unwrap();
    let decoded: ActivityStateV1 = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(decoded, state);
    let rebuilt = ActivityTracker::from_state(&decoded);
    for now in [2_000, 2_000 + ACTIVITY_LINGER_MS * 3] {
        assert_eq!(rebuilt.current(now, false), tracker.current(now, false));
    }
    assert_eq!(rebuilt.used(), tracker.used());
    // Unknown future kinds on the wire are ignored, never coerced.
    let future: ActivityStateV1 = serde_json::from_value(serde_json::json!({
        "open": ["teleport"],
        "touched": [{"kind": "teleport", "at_ms": 5}],
        "latest": {"kind": "teleport", "at_ms": 5}
    }))
    .unwrap();
    assert_eq!(ActivityTracker::from_state(&future).current(5, false), None);
    // An empty state serializes to an empty object.
    assert_eq!(
        serde_json::to_string(&ActivityStateV1::default()).unwrap(),
        "{}"
    );
}
