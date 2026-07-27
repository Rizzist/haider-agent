//! TUI4c — per-surface status derivation (item 12) and the per-session
//! state map (item 13a): leaving keeps a session running IN ITS SLOT, the
//! launcher never wears a background session's state, and re-entering
//! restores exactly.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, Hit, Screen};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{driver_for, key, launcher_model, pump_until, submit};

fn rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn slot(model: &AppModel, id: u64) -> &haider_tui::session::SessionState {
    model
        .sessions
        .iter()
        .find(|entry| entry.id == id)
        .expect("session slot")
}

// ---- Item 12: the launcher never wears a background session's state ----

#[tokio::test(start_paused = true)]
async fn leaving_mid_turn_shows_idle_badge_and_a_running_row() {
    // MUTATION CHECK: derive the badge from the left session again (skip
    // the checkin, or read the slot's projection in the status bar) and
    // the launcher frame shows ⚒ TOOL_RUNNING / a non-zero meter — the
    // owner's screenshot bug.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "walk me through the harness");
    pump_until(&mut driver, &mut rx, &mut model, "mid-turn", |m| {
        m.turn_active && !m.projection.entries().is_empty() && m.projection.badge() != "IDLE"
    })
    .await;
    let sid = model.active_session.expect("attached");
    let busy_badge = model.projection.badge();

    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(model.screen, Screen::Launcher);
    let frame = rows(&model, 118, 34);
    let status = frame.last().cloned().unwrap_or_default();
    let bar = frame
        .iter()
        .rev()
        .find(|row| row.contains("[ "))
        .cloned()
        .unwrap_or(status);
    assert!(
        bar.contains("[ IDLE ]"),
        "launcher badge is IDLE, not the background session's {busy_badge}: {bar:?}"
    );
    assert!(
        bar.contains("0 tok"),
        "the meter follows the surface (sim: branch ? branch.tokens : 0): {bar:?}"
    );
    assert!(
        !bar.contains("· main"),
        "the branch segment renders only with a session: {bar:?}"
    );
    // The busy-ness lives in the ROW (sim tui.js:3252-3266): gold ◉ dot +
    // `running… ·` prefix + the gold `· 1 running` header count.
    assert!(
        frame
            .iter()
            .any(|row| row.contains("◉ walk-me-through") && row.contains("running… ·")),
        "the running row: {frame:?}"
    );
    assert!(
        frame.iter().any(|row| row.contains("· 2 running")),
        "header running count (the L1 seed's live chip + this one)"
    );
    // The turn is still live in the slot.
    assert!(slot(&model, sid).turn_active, "leaving cancels nothing");
}

#[tokio::test(start_paused = true)]
async fn background_events_land_in_the_slot_and_reopen_restores_exactly() {
    // MUTATION CHECK: route background envelopes to the live surface
    // (drop the consume prelude) and the launcher's neutral projection
    // grows entries; drop the slot application instead and the reopened
    // transcript is missing everything streamed while away.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "walk me through the harness");
    pump_until(&mut driver, &mut rx, &mut model, "mid-turn", |m| {
        m.turn_active && !m.projection.entries().is_empty()
    })
    .await;
    let sid = model.active_session.expect("attached");
    // Interrupt the first turn (esc), settle to idle, then run a SECOND
    // turn and leave IT mid-flight via /clear — the leave under test.
    model.handle(key(KeyCode::Esc));
    pump_until(&mut driver, &mut rx, &mut model, "idle", |m| !m.turn_active).await;
    submit(&mut model, "and now the store layer");
    pump_until(&mut driver, &mut rx, &mut model, "second turn live", |m| {
        m.turn_active
    })
    .await;
    let seen = model.projection.entries().len();
    submit(&mut model, "/clear");
    assert_eq!(model.screen, Screen::Launcher);
    assert!(model.projection.entries().is_empty(), "neutral surface");

    // Pump the background turn to ITS end — everything lands in the slot.
    pump_until(
        &mut driver,
        &mut rx,
        &mut model,
        "background turn end",
        |m| !slot(m, sid).turn_active && slot(m, sid).projection.badge() == "IDLE",
    )
    .await;
    assert!(
        slot(&model, sid).projection.entries().len() > seen,
        "the background stream kept landing in the slot"
    );
    assert!(
        model.projection.entries().is_empty(),
        "…and never on the launcher surface"
    );

    // Empty ⏎ re-attaches the same session with everything restored.
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.active_session, Some(sid));
    assert!(model.projection.entries().len() > seen, "restored exactly");
    assert_eq!(model.projection.badge(), "IDLE");
}

#[test]
fn interrupt_marks_only_its_own_session_and_survives_a_round_trip() {
    // MUTATION CHECK: clear `interrupted` in checkin/open (or route the
    // idle(i) marker globally) and either the round-trip loses ⏸ IDLE (i)
    // or a second session wears it.
    let mut model = launcher_model();
    submit(&mut model, "first task here");
    model.requests.clear();
    model.handle(key(KeyCode::Esc)); // mid-turn esc = interrupt
    assert!(model.projection.interrupted(), "idle (i)");
    let first = model.active_session.expect("attached");
    model.handle(key(KeyCode::Esc)); // idle esc = leave
    assert_eq!(model.screen, Screen::Launcher);

    submit(&mut model, "second task here");
    let second = model.active_session.expect("attached");
    assert_ne!(first, second);
    assert!(
        !model.projection.interrupted(),
        "a fresh session never inherits idle (i)"
    );
    // Heads come from the roster in claim order — never a duplicate
    // (sim claimName, tui.js:882-886; seeds hold 0-2).
    assert_eq!(slot(&model, first).head.0, "Hasan");
    assert_eq!(model.session_head.0, "Husayn");

    // Reopen the first: its idle(i) marker survived the round trip.
    model.handle_hit(Hit::BackChip);
    model.handle_hit(Hit::AttachSample("first-task-here".to_owned()));
    assert_eq!(model.active_session, Some(first));
    assert!(
        model.projection.interrupted(),
        "⏸ IDLE (i) restored with the session"
    );
}

#[test]
fn seeded_sessions_round_trip_through_the_map_like_user_ones() {
    // The seeds are REAL sessions now (item 13a): attach, leave, reattach
    // — the seeded transcript, chips and meter ride the same slot laws.
    let mut model = launcher_model();
    model.handle_hit(Hit::AttachSample("l1-remote-projects".to_owned()));
    assert_eq!(model.screen, Screen::Session);
    let tokens = model.projection.context_tokens();
    assert!(tokens > 0, "usage-seeded meter");
    assert_eq!(model.chips.len(), 1, "the live web-index chip came along");
    let entries = model.projection.entries().len();
    assert!(entries > 0, "seed transcript");

    model.handle_hit(Hit::BackChip);
    assert!(model.chips.is_empty(), "neutral surface after leave");
    model.handle_hit(Hit::AttachSample("l1-remote-projects".to_owned()));
    assert_eq!(model.projection.entries().len(), entries);
    assert_eq!(model.projection.context_tokens(), tokens);
    assert_eq!(model.chips.len(), 1);
}

#[test]
fn launcher_typing_starts_fresh_and_the_left_session_keeps_its_row() {
    // MUTATION CHECK: reuse the left session for launcher typing (skip
    // `new_session`) and the new transcript lands on the old session — the
    // /clear fresh-start promise breaks.
    let mut model = launcher_model();
    submit(&mut model, "first task here");
    let first = model.active_session.expect("attached");
    submit(&mut model, "/clear");
    assert_eq!(model.screen, Screen::Launcher);

    submit(&mut model, "totally new work");
    let second = model.active_session.expect("attached");
    assert_ne!(first, second, "a brand-new session id");
    assert_eq!(model.session_name.as_deref(), Some("totally-new-work"));
    assert!(
        model
            .projection
            .entries()
            .iter()
            .all(|entry| !format!("{entry:?}").contains("first task")),
        "no leak from the left session"
    );
    // Both appear in the map; the seeds keep their rows too.
    assert_eq!(model.sessions.len(), 5, "3 seeds + 2 user sessions");
    assert!(
        slot(&model, first).name.as_deref() == Some("first-task-here"),
        "the left session kept its row"
    );
}
