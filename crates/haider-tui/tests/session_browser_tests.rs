//! v0.0.937 `/resume` all-sessions browser: rows are ROSTER truth (no
//! journal replay), ordered by attention — needs-you first, then unseen,
//! then recency — and opening one attaches it, which marks it seen for
//! every surface through the 936 door.

#![allow(clippy::expect_used)]

use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::session::{SessionInteractionModeV1, SessionMetadataV1};
use haider_rpc::{NeedsInputKindWire, NeedsInputWire, SessionSummary};
use haider_tui::app::{AppEvent, AppModel, RuntimeMode, Screen, format_session_age_at};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::projection::RawOutcome;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
        run_id: None,
        seen_at_ms,
        last_activity_ms,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        provider: None,
        workspace_cwd: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: Some(id.to_owned()),
        agent_metrics: None,
        last_model: None,
        cache_lifetime_hit_basis_points: None,
        cache_reread_hit_basis_points: None,
        parent_session_id: None,
        kind: None,
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
        forked_from: None,
    };
    summary.title = Some(id.to_owned());
    summary
}

fn seed(model: &mut AppModel, summary: SessionSummary) {
    model.upsert_live_session(&summary.session_id);
    model.note_summary_counts(&summary);
}

fn metadata(title: &str, cwd: &str, model: &str, created_at_ms: u64) -> SessionMetadataV1 {
    SessionMetadataV1 {
        cwd: cwd.to_owned(),
        provider: "openai".into(),
        account_alias: None,
        model: model.to_owned(),
        max_tokens: 4_096,
        system_prompt_version: Some("test-v1".into()),
        permission_overrides: None,
        interaction_mode: SessionInteractionModeV1::Interactive,
        title: Some(title.to_owned()),
        effort: Some("xhigh".into()),
        fast: false,
        cache_policy: Default::default(),
        agent_type: None,
        context_economy: Default::default(),
        created_at_ms,
    }
}

fn live_envelope(id: &SessionId, committed_at_ms: u64) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("live-{committed_at_ms}")),
        seq: 1,
        session_id: id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("browser-test"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({"type": "browser_activity_probe"}).into(),
    }
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

/// v0.0.938: the browser lists EVERY session on the machine, so it is long by
/// construction and must be navigable beyond one keypress at a time —
/// PageUp/PageDown, Home/End, and the wheel all move the selection, clamped
/// at both ends.
///
/// MUTATION CHECK (executed): drop the `.min(last)` clamp on PageDown/End and
/// the selection runs past the final row, leaving the browser rendering an
/// empty window with nothing selected.
#[test]
fn the_browser_pages_scrolls_and_clamps() {
    let mut model = live_model();
    for index in 0..40_u64 {
        seed(
            &mut model,
            summary(&format!("session-{index:02}"), Some(1), Some(index)),
        );
    }
    model.enter_sessions();
    let last = model.session_browser_rows().len() - 1;
    assert_eq!(last, 39);

    // PageDown moves a page and PageUp comes back.
    model.handle(common::key(KeyCode::PageDown));
    let paged = model.session_browser_sel;
    assert!(paged > 1, "PageDown moves more than one row: {paged}");
    model.handle(common::key(KeyCode::PageUp));
    assert_eq!(model.session_browser_sel, 0, "PageUp returns to the top");

    // End/Home reach both ends exactly.
    model.handle(common::key(KeyCode::End));
    assert_eq!(model.session_browser_sel, last, "End selects the final row");
    model.handle(common::key(KeyCode::Home));
    assert_eq!(model.session_browser_sel, 0);

    // The wheel scrolls the selection too, and clamps at both ends.
    model.handle_wheel(false);
    assert!(
        model.session_browser_sel > 0,
        "wheel down moves the selection"
    );
    for _ in 0..100 {
        model.handle_wheel(false);
    }
    assert_eq!(
        model.session_browser_sel, last,
        "wheel down clamps at the last row, never past it"
    );
    for _ in 0..100 {
        model.handle_wheel(true);
    }
    assert_eq!(model.session_browser_sel, 0, "wheel up clamps at the top");

    // PageDown from the last row stays put rather than overrunning.
    model.handle(common::key(KeyCode::End));
    model.handle(common::key(KeyCode::PageDown));
    assert_eq!(model.session_browser_sel, last);
}

/// Fresh-connect regression for the owner's vanished-session report. Four
/// recency-ordered pages arrive through the shipping live reducer; physical
/// insertion order, ids, and titles all oppose the desired first row.
#[test]
fn fresh_connect_keeps_latest_cold_root_first_across_400_rows_and_hides_children() {
    let mut model = live_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_LINEAGE_V1.to_owned());
    let latest = SessionId::new("session-999-latest");
    let child = SessionId::new("session-998-child");
    let mut summaries = Vec::new();
    for index in 0..400_u64 {
        let id = if index == 0 {
            latest.clone()
        } else if index == 1 {
            child.clone()
        } else {
            SessionId::new(format!("session-{index:03}"))
        };
        let activity = 1_000_000_u64 - index;
        let title = if index == 0 {
            "zzz latest title"
        } else if index == 2 {
            "aaa older title"
        } else {
            "ordinary"
        };
        let mut row = summary(
            id.as_str(),
            (index >= 360).then_some(activity),
            Some(activity),
        );
        row.title = Some(title.into());
        row.last_model = Some("gpt-5.6".into());
        row.workspace_cwd = Some(format!("/work/{index:03}"));
        row.metadata = Some(metadata(
            title,
            &format!("/work/{index:03}"),
            "gpt-5.6",
            900_000_u64 - index,
        ));
        row.kind = Some(if index == 1 {
            haider_rpc::SessionKindWire::Subagent
        } else {
            haider_rpc::SessionKindWire::Root
        });
        row.parent_session_id = (index == 1).then(|| latest.clone());
        summaries.push(row);
    }

    let mut driver = LiveDriver::new("sessionloss-fresh-connect");
    for (page, chunk) in summaries.chunks(100).enumerate() {
        driver.apply(
            &mut model,
            LiveReply::Listed {
                sessions: chunk.to_vec(),
                next_cursor: (page < 3).then(|| format!("opaque-{page}")),
            },
        );
        assert_eq!(
            model.launcher_session_ids().first(),
            Some(&latest),
            "newest row remains first while later pages arrive"
        );
    }

    assert_eq!(
        model.sessions.len(),
        400,
        "child state remains available for nesting"
    );
    let browser = model.session_browser_rows();
    assert_eq!(browser.len(), 399, "subagent is not a top-level row");
    assert_eq!(browser[0].id, latest);
    assert_eq!(browser[0].title, "zzz latest title");
    assert!(browser.iter().all(|row| row.id != child));
    let launcher = model.launcher_session_ids();
    assert_eq!(launcher[0], latest);
    assert!(launcher.iter().all(|id| id != &child));
}

#[test]
fn browser_searches_title_directory_model_and_id_as_you_type() {
    let mut model = live_model();
    let mut alpha = summary("opaque-alpha-id", None, Some(9_000));
    alpha.title = Some("Alpha Mission".into());
    alpha.workspace_cwd = Some("/srv/checkout/payments".into());
    alpha.last_model = Some("gpt-search-special".into());
    alpha.metadata = Some(metadata(
        "Alpha Mission",
        "/srv/checkout/payments",
        "gpt-search-special",
        8_000,
    ));
    seed(&mut model, alpha);
    model
        .sessions
        .iter_mut()
        .find(|row| row.id.as_str() == "opaque-alpha-id")
        .expect("alpha row")
        .title = Some("decorative blurb".into());
    seed(&mut model, summary("other-session", None, Some(8_000)));
    model.enter_sessions();

    for query in ["ALPHA", "payments", "search-special", "opaque-alpha"] {
        for character in query.chars() {
            model.handle(common::key(KeyCode::Char(character)));
        }
        let rows = model.session_browser_rows();
        assert_eq!(rows.len(), 1, "query {query:?} matches exactly one row");
        assert_eq!(rows[0].id.as_str(), "opaque-alpha-id");
        model.handle(common::key(KeyCode::Esc));
        assert_eq!(model.screen, Screen::Sessions, "first esc clears search");
        assert!(model.session_browser_query.is_empty());
    }

    let alpha = SessionId::new("opaque-alpha-id");
    model.open_session(&alpha);
    model.enter_sessions();
    for query in ["Alpha Mission", "/srv/checkout/payments", "search-special"] {
        for character in query.chars() {
            model.handle(common::key(KeyCode::Char(character)));
        }
        let rows = model.session_browser_rows();
        assert_eq!(rows.len(), 1, "active-row query {query:?} matches");
        assert_eq!(rows[0].id, alpha);
        assert_eq!(rows[0].title, "Alpha Mission", "blurb never wins title");
        model.handle(common::key(KeyCode::Esc));
    }
}

#[test]
fn equal_recency_uses_creation_and_never_title() {
    let mut model = live_model();
    let mut alpha = summary("session-a", None, Some(5_000));
    alpha.metadata = Some(metadata("same", "/a", "gpt", 2_000));
    let mut zulu = summary("session-z", None, Some(5_000));
    zulu.metadata = Some(metadata("same", "/z", "gpt", 2_000));
    seed(&mut model, alpha);
    seed(&mut model, zulu);
    let before = model
        .session_browser_rows()
        .into_iter()
        .map(|row| row.id)
        .collect::<Vec<_>>();
    model
        .sessions
        .iter_mut()
        .find(|row| row.id.as_str() == "session-z")
        .expect("z row")
        .name = Some("zzz title".into());
    model
        .sessions
        .iter_mut()
        .find(|row| row.id.as_str() == "session-a")
        .expect("a row")
        .name = Some("aaa title".into());
    let after = model
        .session_browser_rows()
        .into_iter()
        .map(|row| row.id)
        .collect::<Vec<_>>();
    assert_eq!(before, after, "title changes cannot reorder exact ties");
    assert_eq!(after[0].as_str(), "session-z", "stable roster tie order");
}

#[test]
fn durable_activity_formats_cold_session_age() {
    assert_eq!(format_session_age_at(1_000_000, 880_000), "2m ago");
    assert_eq!(format_session_age_at(1_000_000, 1_100_000), "now");

    let mut model = live_model();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64;
    let activity_ms = now_ms.saturating_sub(120_000);
    seed(
        &mut model,
        summary("cold-age", Some(activity_ms), Some(activity_ms)),
    );
    assert_eq!(model.session_browser_rows()[0].ago, "2m ago");
    assert_eq!(
        model.session_display_age(&SessionId::new("cold-age"), "stale fallback"),
        "2m ago"
    );
}

#[test]
fn live_event_recency_beats_a_later_stale_summary() {
    let mut model = live_model();
    let stale = summary("live-freshness", None, Some(100));
    seed(&mut model, stale.clone());
    let id = SessionId::new("live-freshness");
    assert_eq!(
        model.route_raw(&live_envelope(&id, 200)),
        RawOutcome::Applied
    );
    model.note_summary_counts(&stale);
    assert_eq!(model.session_browser_rows()[0].last_activity_ms, Some(200));
}

#[test]
fn sessions_search_rejects_alt_text_and_preserves_global_ctrl_c() {
    let mut model = live_model();
    seed(&mut model, summary("session-one", None, Some(1)));
    model.enter_sessions();
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::ALT,
    )));
    assert!(model.session_browser_query.is_empty());
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(model.screen, Screen::Launcher);
    assert!(model.session_browser_query.is_empty());
}

#[test]
fn bare_sessions_command_opens_the_full_browser() {
    let mut model = live_model();
    seed(&mut model, summary("session-one", None, Some(1)));
    common::run_slash(&mut model, "/sessions");
    assert_eq!(model.screen, Screen::Sessions);
    assert_eq!(model.session_browser_rows().len(), 1);
}
