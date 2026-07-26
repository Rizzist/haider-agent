//! The demo script must drive the projection through the full story —
//! the projection is the oracle, exactly as it will be at render time.
#![allow(clippy::expect_used)]

use haider_protocol::item::TurnItem;
use haider_tui::mock::{DEMO_CHECKS, demo_script};
use haider_tui::projection::{SessionProjection, TranscriptEntry};

#[test]
fn demo_script_walks_boot_to_done() {
    let script = demo_script();
    let mut projection = SessionProjection::new();
    let mut badges = Vec::new();
    for payload in &script {
        projection.apply(payload);
        let badge = projection.badge();
        if badges.last() != Some(&badge) {
            badges.push(badge);
        }
    }

    assert_eq!(
        badges,
        vec![
            "◌ STARTING",
            "IDLE",
            "◌ QUEUED",
            "● THINKING",
            "▮ STREAMING",
            "⚒ TOOL_RUNNING",
            "IDLE",
        ],
        "badge story: boot → idle → turn → done"
    );
}

#[test]
fn demo_script_produces_the_expected_transcript() {
    let mut projection = SessionProjection::new();
    for payload in &demo_script() {
        projection.apply(payload);
    }

    let entries = projection.entries();
    assert!(matches!(
        entries[0],
        TranscriptEntry::User { ref text, .. } if text.contains("boundary test")
    ));
    let kinds: Vec<&'static str> = entries
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::User { .. } => "user",
            TranscriptEntry::Item(block) => match block.item {
                TurnItem::AgentMessage { .. } => "agent",
                TurnItem::ToolCall { .. } => "tool",
                TurnItem::FileChange { .. } => "file",
                TurnItem::Plan { .. } => "plan",
                _ => "other",
            },
        })
        .collect();
    assert_eq!(kinds, vec!["user", "agent", "tool", "file", "plan"]);

    // Everything settled: nothing left streaming, plan unpinned, tokens live.
    assert!(entries.iter().all(|entry| match entry {
        TranscriptEntry::Item(block) => !block.streaming,
        TranscriptEntry::User { .. } => true,
    }));
    let todos = projection.todos().expect("plan ran");
    assert!(!todos.pinned);
    assert_eq!(todos.done_count(), 3);
    assert_eq!(projection.context_tokens(), 33_400);
    assert_eq!(projection.boot_checks(), None, "boot finished");
    assert_eq!(projection.orphan_deltas(), 0);
}

#[test]
fn demo_boot_checks_progress_one_at_a_time() {
    let script = demo_script();
    let mut projection = SessionProjection::new();
    let mut seen_done_counts = Vec::new();
    for payload in &script {
        projection.apply(payload);
        if let Some(checks) = projection.boot_checks() {
            seen_done_counts.push(checks.iter().filter(|c| c.ok).count());
        }
    }
    assert_eq!(seen_done_counts, vec![0, 1, 2, 3, DEMO_CHECKS.len()]);
}
