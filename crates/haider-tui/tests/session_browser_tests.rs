//! v0.0.937 `/resume` all-sessions browser: rows are ROSTER truth (no
//! journal replay), ordered by attention — needs-you first, then unseen,
//! then recency — and opening one attaches it, which marks it seen for
//! every surface through the 936 door.

#![allow(clippy::expect_used)]

use haider_protocol::ids::SessionId;
use haider_rpc::{NeedsInputKindWire, NeedsInputWire, SessionSummary};
use haider_tui::app::{AppModel, RuntimeMode, Screen};
use ratatui::crossterm::event::KeyCode;

mod common;
use common::launcher_model;

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_SESSION_SEEN_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_NEEDS_INPUT_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.sessions.clear();
    model
}

fn summary(id: &str, seen_at_ms: Option<u64>, last_activity_ms: Option<u64>) -> SessionSummary {
    let mut summary = SessionSummary {
        session_id: SessionId::new(id),
        head_seq: 4,
        worker_generation: 7,
        run_state: None,
        seen_at_ms,
        last_activity_ms,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        workspace_cwd: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: Some(id.to_owned()),
        agent_metrics: None,
        last_model: None,
        parent_session_id: None,
        kind: None,
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
    };
    summary.title = Some(id.to_owned());
    summary
}

fn seed(model: &mut AppModel, summary: SessionSummary) {
    model.upsert_live_session(&summary.session_id);
    model.note_summary_counts(&summary);
}

/// MUTATION CHECK (executed): swap the tier ranks in
/// `session_browser_rows`'s sort (or drop the tier key entirely and sort by
/// recency alone) and the ordering assertion fails — a session needing a
/// human would sink below merely-unseen and settled rows.
#[test]
fn rows_order_needs_you_then_unseen_then_recent() {
    let mut model = live_model();
    // A settled row with the MOST RECENT activity: recency alone would put
    // it first, so it proves the attention tiers outrank recency.
    seed(
        &mut model,
        summary("settled-newest", Some(9_000), Some(8_000)),
    );
    seed(
        &mut model,
        summary("unseen-older", Some(1_000), Some(2_000)),
    );
    seed(
        &mut model,
        summary("unseen-newer", Some(1_000), Some(5_000)),
    );
    let mut parked = summary("needs-you", Some(9_000), Some(3_000));
    parked.needs_input = Some(NeedsInputWire {
        kind: NeedsInputKindWire::Recovery,
        title: "Effect outcome unknown".into(),
        safe_body: Vec::new(),
        menu_id: None,
        request_seq: None,
        worker_generation: None,
        since_ms: None,
        options: Vec::new(),
        secret_answer: false,
    });
    seed(&mut model, parked);

    let rows = model.session_browser_rows();
    let order: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        order,
        vec![
            "needs-you",
            "unseen-newer",
            "unseen-older",
            "settled-newest"
        ],
        "attention tiers outrank recency, recency orders within a tier"
    );
    assert!(rows[0].needs_input.is_some());
    assert!(rows[1].unseen && rows[2].unseen);
    assert!(!rows[3].unseen, "activity older than seen is settled");
}

/// The browser is reachable, selects, opens, and returns — the whole
/// keyboard contract in one pass.
///
/// MUTATION CHECK (executed): make `enter_sessions` skip recording the
/// return screen and the esc assertion fails (it would fall back to the
/// launcher from a session); drop the Enter arm and the attach fails.
#[test]
fn the_browser_opens_selects_attaches_and_returns() {
    let mut model = live_model();
    seed(&mut model, summary("session-alpha", Some(1), Some(2)));
    seed(&mut model, summary("session-beta", Some(1), Some(9)));

    model.enter_sessions();
    assert_eq!(model.screen, Screen::Sessions);
    // Ordered by recency inside the unseen tier: beta leads.
    assert_eq!(model.session_browser_rows()[0].id.as_str(), "session-beta");

    // ↓ then ⏎ opens the SECOND row.
    model.handle(common::key(KeyCode::Down));
    assert_eq!(model.session_browser_sel, 1);
    model.handle(common::key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(
        model.active_session.as_ref().map(SessionId::as_str),
        Some("session-alpha"),
        "⏎ attaches the SELECTED row"
    );

    // Re-entering from a session and pressing esc returns THERE, not to
    // the launcher.
    model.enter_sessions();
    assert_eq!(model.screen, Screen::Sessions);
    model.handle(common::key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session, "esc returns whence it came");
}

/// The demo fabricates nothing: `/resume` is live-only, and a demo model
/// flashes instead of rendering invented rows.
#[test]
fn the_browser_is_live_only() {
    let mut model = launcher_model();
    assert!(model.mode.fabricates_locally());
    model.enter_sessions();
    assert_ne!(model.screen, Screen::Sessions);
    assert!(
        model
            .flash
            .as_ref()
            .is_some_and(|flash| flash.contains("live only")),
        "the demo says so honestly instead of fabricating a roster"
    );
}
