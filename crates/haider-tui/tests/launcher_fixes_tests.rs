//! UI launcher fixes (owner screenshots, v0.0.65 follow-up):
//!
//! 1. the settled launcher's CONTENT BLOCK (recent-session roster +
//!    Aura/Accounts/Peers, the ~70-col capped column) centers horizontally
//!    in the frame — header band and composer band stay full-width, widths
//!    ≤ the cap keep the old left-anchored geometry, and the hit map moves
//!    WITH the paint (one rect for both);
//! 2. roster rows show REAL turns/tokens WITHOUT attaching, hydrated from
//!    the additive `session.list` summary fields — tolerantly (an older
//!    daemon degrades to the old display, never a fabricated count), and
//!    never outranking fresher checkout/checkin truth.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::context::ContextFootprintTruth;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::state::RunState;
use haider_rpc::{AttachmentId, SessionSummary};
use haider_tui::app::{AppModel, Hit, LauncherRow, RuntimeMode, Screen};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

mod common;
use common::launcher_model;

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rows.push(line);
    }
    (rows, hits)
}

/// The y-coordinate of the first rendered row containing `needle`.
fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row containing {needle:?} not rendered")),
    )
    .expect("row fits u16")
}

/// The cell column where `needle` starts within a rendered row.
fn col_of(row: &str, needle: &str) -> u16 {
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("column of {needle:?} not found in row {row:?}"));
    u16::try_from(row[..byte].chars().count()).expect("col fits u16")
}

/// The unique hit rect carrying `hit`.
fn rect_for(hits: &[(Rect, Hit)], hit: &Hit) -> Rect {
    let mut matches = hits.iter().filter(|(_, h)| h == hit);
    let (rect, _) = matches
        .next()
        .unwrap_or_else(|| panic!("no hit region for {hit:?}"));
    assert!(
        matches.next().is_none(),
        "duplicate hit regions for {hit:?}"
    );
    *rect
}

/// The runtime's first-match containment lookup (`hit_at` in the event
/// loop), so a click in these tests resolves exactly as a real one.
fn hit_at(hits: &[(Rect, Hit)], x: u16, y: u16) -> Option<Hit> {
    hits.iter()
        .find(|(rect, _)| {
            x >= rect.x
                && x < rect.x.saturating_add(rect.width)
                && y >= rect.y
                && y < rect.y.saturating_add(rect.height)
        })
        .map(|(_, hit)| hit.clone())
}

// ---- FIX 1: horizontal centering ------------------------------------

/// MUTATION CHECKS: (a) force `center_pad` to 0 in `render_launcher` and
/// the 130/80-col column pins fail; (b) shift only the paint rect (keep
/// hits on the unshifted area) and the exact `rect.x` pins fail; (c) shift
/// only the hits and the click-through resolution fails.
#[test]
fn launcher_content_block_is_centered() {
    let model = launcher_model();
    // 130 cols: the 70-col cap leaves (130-70)/2 = 30 pad cells; every
    // block row leads with the one-cell rail, so the head TEXT sits at 31.
    let (rows, hits) = draw(&model, 130, 34);
    let head_y = row_of(&rows, "recent sessions");
    assert_eq!(col_of(&rows[head_y as usize], "recent sessions"), 31);
    let billing = common::session_named(&model, "billing-service");
    let billing_hit = Hit::AttachSession(billing.clone());
    let rect = rect_for(&hits, &billing_hit);
    assert_eq!(rect.x, 30, "hit rect moved WITH the centered paint");
    assert_eq!(rect.height, 1);
    let name_y = row_of(&rows, "billing-service");
    assert_eq!(rect.y, name_y, "hit row is the painted row");
    let name_x = col_of(&rows[name_y as usize], "billing-service");
    assert_eq!(
        hit_at(&hits, name_x, name_y),
        Some(billing_hit.clone()),
        "a click on the centered name resolves to the row's session"
    );
    // The extra rows center with the block and stay value-carrying.
    let aura_y = row_of(&rows, "◉ Aura");
    let aura_x = col_of(&rows[aura_y as usize], "Aura");
    assert_eq!(
        hit_at(&hits, aura_x, aura_y),
        Some(Hit::ExtraRow(LauncherRow::Aura))
    );
    // Header band and composer band stay full-width: the header's closing
    // rule (above the block's breathing row) spans from column 0 and the
    // composer sigil keeps its left-anchored cell.
    assert!(rows[(head_y - 2) as usize].starts_with('─'));
    let composer_y = row_of(&rows, "start a session");
    assert!(col_of(&rows[composer_y as usize], "❯") < 5);

    // 80 cols: pad (80-70)/2 = 5.
    let (rows, hits) = draw(&model, 80, 34);
    let head_y = row_of(&rows, "recent sessions");
    assert_eq!(col_of(&rows[head_y as usize], "recent sessions"), 6);
    let rect = rect_for(&hits, &billing_hit);
    assert_eq!(rect.x, 5);
    let name_y = row_of(&rows, "billing-service");
    let name_x = col_of(&rows[name_y as usize], "billing-service");
    assert_eq!(hit_at(&hits, name_x, name_y), Some(billing_hit.clone()));

    // ≤ the cap: pad 0 — exactly the old left-anchored geometry.
    let (rows, hits) = draw(&model, 70, 34);
    let head_y = row_of(&rows, "recent sessions");
    assert_eq!(col_of(&rows[head_y as usize], "recent sessions"), 1);
    assert_eq!(rect_for(&hits, &billing_hit).x, 0);

    // The click ACTS: attach through the centered hit at 130 cols.
    let mut model = model;
    let (rows, hits) = draw(&model, 130, 34);
    let name_y = row_of(&rows, "billing-service");
    let name_x = col_of(&rows[name_y as usize], "billing-service");
    let hit = hit_at(&hits, name_x, name_y).expect("click lands");
    model.handle_hit(hit);
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.active_session, Some(billing));
}

// ---- FIX 2: summary-hydrated roster counts ---------------------------

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model
}

fn sid(n: usize) -> SessionId {
    SessionId::new(format!("s-{n}"))
}

fn attachment(n: usize) -> AttachmentId {
    AttachmentId::new(format!("att-{n}"))
}

fn summary(n: usize, head_seq: u64) -> SessionSummary {
    SessionSummary {
        session_id: sid(n),
        head_seq,
        worker_generation: 7,
        metadata: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
    }
}

fn listed(driver: &mut LiveDriver, model: &mut AppModel, sessions: Vec<SessionSummary>) {
    driver.apply(
        model,
        LiveReply::Listed {
            sessions,
            next_cursor: None,
        },
    );
}

fn envelope(session: &SessionId, seq: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("live-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

/// MUTATION CHECKS: (a) make `SessionState::turns` ignore the summary and
/// the `12 turns` pin fails; (b) drop the Estimated marker from
/// `row_tokens` and the `~48k` pin fails.
#[test]
fn roster_shows_summary_counts_without_attach() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let mut estimated = summary(0, 9);
    estimated.turn_count = Some(12);
    estimated.footprint_tokens = Some(48_200);
    estimated.footprint_truth = Some(ContextFootprintTruth::Estimated);
    let mut exact = summary(1, 4);
    exact.turn_count = Some(3);
    exact.footprint_tokens = Some(9_100);
    exact.footprint_truth = Some(ContextFootprintTruth::Exact);
    listed(&mut driver, &mut model, vec![estimated, exact]);
    // No attach happened — the counts are summary truth, not replay.
    assert!(!driver.is_attached(&sid(0)));
    assert!(!driver.is_attached(&sid(1)));
    let (rows, _) = draw(&model, 130, 34);
    let est_row = &rows[row_of(&rows, "12 turns") as usize];
    assert!(
        est_row.contains("12 turns · ~48k tok"),
        "estimated footprint wears the honest ~ prefix: {est_row:?}"
    );
    let exact_row = &rows[row_of(&rows, "3 turns") as usize];
    assert!(
        exact_row.contains("3 turns · 9.1k tok"),
        "exact footprint shows unprefixed: {exact_row:?}"
    );
    assert!(!exact_row.contains('~'), "no ~ on an Exact snapshot");
}

/// MUTATION CHECK: drop `note_summary_counts`' both-fields-absent guard
/// (store empty counts anyway) and the stale-sticky pin fails — the later
/// field-less list wipes the row back to `0 turns`.
#[test]
fn older_daemon_without_fields_degrades_honestly() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    // An older daemon's summary: no additive fields at all.
    listed(&mut driver, &mut model, vec![summary(0, 9)]);
    assert_eq!(
        model.sessions[0].summary_counts, None,
        "nothing stored — nothing to fabricate from"
    );
    let (rows, _) = draw(&model, 130, 34);
    let row = &rows[row_of(&rows, "session ▸") as usize];
    assert!(
        row.contains("0 turns · 0 tok"),
        "pre-field display, exactly as before: {row:?}"
    );
    // A value-carrying summary lands…
    let mut counted = summary(0, 10);
    counted.turn_count = Some(7);
    listed(&mut driver, &mut model, vec![counted]);
    let (rows, _) = draw(&model, 130, 34);
    assert!(rows[row_of(&rows, "session ▸") as usize].contains("7 turns"));
    // …and a LATER field-less list (daemon rolled back) keeps the real
    // values instead of wiping them to a fabricated zero.
    listed(&mut driver, &mut model, vec![summary(0, 12)]);
    let (rows, _) = draw(&model, 130, 34);
    assert!(
        rows[row_of(&rows, "session ▸") as usize].contains("7 turns"),
        "stale-sticky beats a wipe — absent fields never overwrite"
    );
}

/// MUTATION CHECK: relax `SessionState::summary_is_fresher` from
/// `head_seq > applied` to `>=` and the post-checkin pin fails — the row
/// re-dresses in the stale summary's `99 turns`.
#[test]
fn checkin_values_beat_stale_summaries() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let mut stale = summary(0, 2);
    stale.turn_count = Some(99);
    stale.footprint_tokens = Some(999_000);
    stale.footprint_truth = Some(ContextFootprintTruth::Estimated);
    listed(&mut driver, &mut model, vec![stale.clone()]);
    // Hydrated: the row shows the summary's counts before any attach.
    let (rows, _) = draw(&model, 130, 34);
    assert!(rows[row_of(&rows, "session ▸") as usize].contains("99 turns"));
    // Open the session (checkout), attach, and replay through the head.
    model.handle_hit(Hit::AttachSession(sid(0)));
    assert_eq!(model.screen, Screen::Session);
    let commands = driver.sync_selection(&model);
    assert!(!commands.is_empty(), "selection attaches");
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(0),
            attachment: attachment(0),
            worker_generation: 7,
            replay_through_seq: 2,
        },
    );
    for (seq, payload) in [
        (
            1,
            EventPayload::UserMessage {
                text: "one real turn".to_owned(),
                attachments: vec![],
                mode: haider_protocol::DeliveryMode::Steer,
            },
        ),
        (2, EventPayload::RunState(RunState::Done)),
    ] {
        driver.apply(
            &mut model,
            LiveReply::Event {
                attachment: attachment(0),
                session: sid(0),
                envelope: Box::new(envelope(&sid(0), seq, &payload)),
            },
        );
    }
    // Back to the launcher: checkin. The projection applied THROUGH the
    // summary's head, so the live truth (1 turn) outranks its 99.
    model.back_to_launcher();
    let (rows, _) = draw(&model, 130, 34);
    let row = &rows[row_of(&rows, "session ▸") as usize];
    assert!(
        row.contains("1 turn ·") && !row.contains("99 turns"),
        "checkin values beat the stale summary: {row:?}"
    );
    // The SAME stale summary re-listed changes nothing.
    listed(&mut driver, &mut model, vec![stale]);
    let (rows, _) = draw(&model, 130, 34);
    assert!(!rows[row_of(&rows, "session ▸") as usize].contains("99 turns"));
    // A summary STRICTLY AHEAD of everything applied wins again — the
    // freshness law cuts both ways.
    let mut fresher = summary(0, 5);
    fresher.turn_count = Some(5);
    listed(&mut driver, &mut model, vec![fresher]);
    let (rows, _) = draw(&model, 130, 34);
    assert!(
        rows[row_of(&rows, "session ▸") as usize].contains("5 turns"),
        "a summary ahead of the applied cursor is the fresher truth"
    );
}
