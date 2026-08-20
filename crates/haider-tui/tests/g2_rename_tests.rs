//! G2 — session rename in the TUI: `/rename` rides the receipted
//! `session.rename` wire (the header moves only on the daemon's NORMALIZED
//! reply), launcher rows and `/sessions` hydrate their names from the
//! additive `SessionSummary.title`, and every refusal is an honest notice.
#![allow(clippy::expect_used)]

use haider_protocol::ids::SessionId;
use haider_rpc::{RequestBody, ResponseBody};
use haider_tui::app::{AppModel, AppRequest, RuntimeMode, Screen};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver};

mod common;
use common::{launcher_model, run_slash};

fn sid() -> SessionId {
    SessionId::new("s-rename")
}

/// A live attached session with `session_rename_v1` advertised.
fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_SESSION_RENAME_V1.to_owned()]
        .into_iter()
        .collect();
    model.daemon_version = Some("0.0.71".to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model
}

fn summary(
    session_id: &SessionId,
    head_seq: u64,
    title: Option<&str>,
) -> haider_rpc::SessionSummary {
    haider_rpc::SessionSummary {
        session_id: session_id.clone(),
        head_seq,
        worker_generation: 7,
        metadata: None,
        workspace_cwd: None,
        turn_count: Some(2),
        footprint_tokens: None,
        footprint_truth: None,
        title: title.map(str::to_owned),
        agent_metrics: None,
        last_model: None,
        parent_session_id: None,
        kind: None,
    }
}

fn drain(driver: &mut LiveDriver, model: &mut AppModel) -> Vec<LiveCommand> {
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    let mut commands = Vec::new();
    for request in requests {
        commands.extend(driver.handle_request(model, request));
    }
    commands
}

// ---- law 1: /rename rides the wire and the reply moves the header ------

/// LAW (G2 TUI): `/rename <name>` issues the durable `session.rename` at
/// the session's generation, the exact wire body carries the title, and
/// ONLY the correlated NORMALIZED reply sets `session_name` (optimism
/// forbidden) — daemon truth, never an echo.
///
/// MUTATION CHECK: set `session_name` at issuance, or map the reply's
/// title from the pending request instead of the response. Expected
/// RUNTIME failure: the pre-reply assertion or the normalized-title
/// assertion below.
#[test]
fn rename_issues_the_wire_command_and_applies_the_normalized_reply() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    run_slash(&mut model, "/rename  Parser rewrite plan ");
    let commands = drain(&mut driver, &mut model);
    let rename = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::Rename { .. }))
        .expect("the rename command")
        .clone();
    let LiveCommand::Rename { session, title, .. } = rename.clone() else {
        unreachable!()
    };
    assert_eq!(session, sid());
    assert_eq!(title, "Parser rewrite plan", "reducer-trimmed");
    // Optimism forbidden: nothing moved before the reply.
    assert_eq!(model.session_name, None);

    // EXACT WIRE: the method body names session.rename and the title.
    let body = request_body(rename.clone());
    let encoded = serde_json::to_value(&body).expect("encodes");
    assert_eq!(
        encoded.get("method").and_then(|v| v.as_str()),
        Some("session.rename")
    );
    assert_eq!(
        encoded.get("title").and_then(|v| v.as_str()),
        Some("Parser rewrite plan")
    );
    let RequestBody::SessionRename { command_id, .. } = body else {
        panic!("expected a session.rename request");
    };

    // The reply carries the DAEMON-normalized title (here: further
    // shortened), and that is what the header shows.
    let context = CommandContext::of(&rename);
    let replies = map_response(
        &context,
        ResponseBody::SessionRename {
            session_id: sid(),
            title: Some("Parser rewrite".into()),
            renamed_seq: 9,
            worker_generation: 7,
        },
    );
    for reply in replies {
        driver.apply(&mut model, reply);
    }
    assert_eq!(model.session_name.as_deref(), Some("Parser rewrite"));
    assert_eq!(model.flash.as_deref(), Some("· renamed → Parser rewrite"));
    let _ = command_id;
}

// ---- law 2: refusals are honest notices --------------------------------

/// LAW (G2 TUI refusals): bare `/rename` is a usage flash (clearing is not
/// offered), a feature-ungated daemon gets the stale-daemon notice with NO
/// request issued, and the launcher gets the session-only notice.
#[test]
fn rename_refusals_are_honest_notices_with_no_command() {
    // Bare /rename → usage.
    let mut model = live_model();
    run_slash(&mut model, "/rename");
    assert_eq!(model.flash.as_deref(), Some("· /rename — give a name"));
    assert!(model.requests.is_empty(), "no request for bare /rename");

    // Feature-ungated daemon → stale note, nothing issued.
    let mut model = live_model();
    model.daemon_features.clear();
    run_slash(&mut model, "/rename fresh-name");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("daemon")),
        "flash names the stale daemon: {:?}",
        model.flash
    );
    assert!(model.requests.is_empty(), "no request without the feature");

    // Launcher → session only.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    assert_eq!(model.screen, Screen::Launcher);
    run_slash(&mut model, "/rename anywhere");
    assert_eq!(model.flash.as_deref(), Some("· /rename — session only"));
}

// ---- law 3 (LB5): the wire title names launcher rows and /sessions -----

/// LAW (LB5): a `session.list` summary's additive `title` hydrates the
/// roster row's NAME — the launcher row and `/sessions` render the wire
/// title — while an absent title (older daemon) hydrates nothing, and a
/// summary for the ATTACHED session updates the live header instead of the
/// neutral checked-out slot.
///
/// MUTATION CHECK: drop the title hydration from `note_summary_counts`.
/// Expected RUNTIME failure: the row keeps its nameless "session" fallback
/// below.
#[test]
fn session_list_title_hydrates_launcher_rows_and_sessions_listing() {
    use haider_tui::live::LiveReply;

    // A background (unattached) row takes the wire title.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    let background = SessionId::new("s-background");
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(&background, 5, Some("Parser rewrite"))],
            next_cursor: None,
        },
    );
    let entry = model
        .sessions
        .iter()
        .find(|entry| entry.id == background)
        .expect("listed row");
    assert_eq!(entry.name.as_deref(), Some("Parser rewrite"));

    // /sessions renders the same name (the listing reads `entry.name`).
    run_slash(&mut model, "/sessions");
    let (_, listing) = model
        .launcher_shellout
        .clone()
        .expect("launcher shellout listing");
    assert!(
        listing.contains("Parser rewrite"),
        "/sessions names the row: {listing}"
    );

    // An absent title hydrates NOTHING (older daemon tolerance).
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(&background, 6, None)],
            next_cursor: None,
        },
    );
    let entry = model
        .sessions
        .iter()
        .find(|entry| entry.id == background)
        .expect("listed row");
    assert_eq!(
        entry.name.as_deref(),
        Some("Parser rewrite"),
        "absence must not clear a hydrated name"
    );

    // The ATTACHED session's summary lands on the live header, not the
    // neutral checked-out slot.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(&sid(), 7, Some("auto-titled"))],
            next_cursor: None,
        },
    );
    assert_eq!(model.session_name.as_deref(), Some("auto-titled"));
}

// ---- law 4: demo renames locally ---------------------------------------

/// Demo `/rename` fabricates locally (its world IS local, like the model
/// picker): the header updates immediately with the honest flash and no
/// request rides out.
#[test]
fn demo_rename_is_local() {
    let mut model = launcher_model();
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    run_slash(&mut model, "/rename local-name");
    assert_eq!(model.session_name.as_deref(), Some("local-name"));
    assert_eq!(model.flash.as_deref(), Some("· renamed → local-name"));
    assert!(model.requests.is_empty(), "demo issues no wire command");
}

// ---- model truth (owner 2026-08-15): rows wear the session's ACTUAL model ----

/// LAW: the roster row's MODEL comes from the daemon's journal-folded
/// `last_model` — the model the session ACTUALLY runs — never this
/// client's own identity (the old seed made every row wear the CLIENT's
/// current model, e.g. gpt-5.6-sol over a DeepSeek session).
///
/// MUTATION CHECK: drop the `last_model` hydration from the summary apply
/// (or stamp rows from `identity.model_short` again). Expected RUNTIME
/// failure: the background row below keeps the client-seeded model.
#[test]
fn session_list_last_model_hydrates_roster_rows() {
    use haider_tui::live::LiveReply;

    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.identity.model_short = "gpt-5.6-sol".to_owned();
    let background = SessionId::new("s-model-truth");
    let mut driver = LiveDriver::new("test");
    let mut listed = summary(&background, 5, None);
    listed.last_model = Some("deepseek-v4-flash".to_owned());
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![listed],
            next_cursor: None,
        },
    );
    let entry = model
        .sessions
        .iter()
        .find(|entry| entry.id == background)
        .expect("listed row");
    assert_eq!(
        entry.model_short, "deepseek-v4-flash",
        "the row wears the session's ACTUAL model, not the client's"
    );

    // Absence hydrates nothing (older-daemon tolerance): the row keeps
    // its hydrated model instead of regressing to the client's.
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(&background, 6, None)],
            next_cursor: None,
        },
    );
    let entry = model
        .sessions
        .iter()
        .find(|entry| entry.id == background)
        .expect("row survives");
    assert_eq!(entry.model_short, "deepseek-v4-flash");
}
