//! TUI4d — item 14: the sim's animations, terminal-faithful. One shared
//! phase clock on the model (`anim_phase`), armed by `AppModel::animated`
//! ONLY while a pulsing element is on screen; render alternates each
//! pulsing span between full ink and the sim's 0.35-opacity midpoint
//! (`pulse` keyframes, tui.js:3943-3946), with the launcher rail cycling
//! gold→maroon→gold (`railShimmer`, tui.js:3948-3951).
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::RunState;
use haider_tui::app::{AppEvent, AppModel, Screen};
use haider_tui::projection::badge_pulses;
use haider_tui::render::render;
use haider_tui::script::{AuraState, ChipDisplayState, ChipSeed};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

mod common;
use common::launcher_model;

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, terminal)
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row not found: {needle}")),
    )
    .expect("row index")
}

fn col_of(row: &str, needle: &str) -> u16 {
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in {row:?}"));
    u16::try_from(row[..byte].chars().count()).expect("col")
}

/// The cell fg at (needle's first char) on the needle's row.
fn fg_at(model: &AppModel, needle: &str) -> Color {
    let (rows, terminal) = draw(model, 118, 34);
    let y = row_of(&rows, needle);
    let x = col_of(&rows[y as usize], needle);
    terminal.backend().buffer()[(x, y)].fg
}

fn seed_chip(state: ChipDisplayState) -> haider_tui::app::ChipModel {
    haider_tui::app::ChipModel::from_seed(ChipSeed {
        agent: "anim-chip".to_owned(),
        parent: None,
        ros: None,
        callsign: "Salman".to_owned(),
        hon: "(r)",
        full: "Salman al-Farsi".to_owned(),
        name: "task".to_owned(),
        model: "gpt-5.6".to_owned(),
        device: "local".to_owned(),
        state,
        tokens: 100,
        prefill: vec![],
    })
}

// ---- The thinking line breathes on the shared clock ----

#[test]
fn phase_toggle_alternates_the_thinking_line() {
    // MUTATION CHECK: render the thinking tail from a static
    // `theme.gold_style()` again (drop `thinking_line`'s phase inputs) and
    // both the glyph swap and the ink dip below vanish — either assert
    // fails.
    let mut model = launcher_model();
    common::hit_session_named(&mut model, "billing-service");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Thinking,
    ))));
    let theme = model.theme.theme();

    model.anim_phase = 0;
    let on = fg_at(&model, "● thinking…");
    assert_eq!(on, Color::from(theme.gold), "ON phase: full gold");

    model.anim_phase = 1;
    let (rows, terminal) = draw(&model, 118, 34);
    let y = row_of(&rows, "◌ thinking…");
    let x = col_of(&rows[y as usize], "◌");
    let off = terminal.backend().buffer()[(x, y)].fg;
    assert_eq!(
        off,
        Color::from(theme.gold.over(theme.bg, 350)),
        "OFF phase: the sim's 0.35-opacity midpoint over the ground"
    );
    assert_ne!(on, off, "the two phases render differently");
    assert!(
        !rows.iter().any(|row| row.contains("● thinking…")),
        "the dot breathes ● ↔ ◌ with the ink"
    );
}

// ---- The clock is armed IFF something on screen pulses ----

#[test]
fn the_phase_clock_is_armed_iff_an_animated_state_exists() {
    // MUTATION CHECK: remove ANY registration arm from
    // `AppModel::animated` (the thinking check, the busy-launcher check,
    // the chip check, the todos check, the listening check…) and its case
    // below fails — an animated state that never registers never moves,
    // which is exactly the bug class this predicate exists to prevent.
    let mut model = launcher_model();
    // The seeded launcher animates: the L1 seed's live web-index chip
    // makes its row busy (gold dot pulse + rail shimmer).
    assert!(model.animated(), "busy launcher row");

    // Silence the seeds → a fully idle launcher takes ZERO wakeups.
    for session in &mut model.sessions {
        session.chips.clear();
    }
    assert!(!model.animated(), "idle launcher is still");

    // An idle session (no thinking, no tools, no todos, no chips): still.
    common::hit_session_named(&mut model, "billing-service");
    assert_eq!(model.screen, Screen::Session);
    assert!(!model.animated(), "idle session is still");

    // Thinking arms the clock; the terminal run state disarms it on the
    // very next predicate read — the runtime's gate closes within one
    // phase of the last animated state ending.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Thinking,
    ))));
    assert!(model.animated(), "thinking pulses");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));
    assert!(!model.animated(), "the clock stops with the turn");

    // A running tool row pulses until it completes.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Started {
            item_id: ItemId::new("anim-tool"),
            item: TurnItem::ToolCall {
                call_id: "anim-tool".to_owned(),
                name: "fs_read".to_owned(),
                args: serde_json::json!({}),
                status: ToolStatus::InProgress,
            },
        },
    ))));
    assert!(model.animated(), "running tool glyph pulses");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Completed {
            item_id: ItemId::new("anim-tool"),
            item: TurnItem::ToolCall {
                call_id: "anim-tool".to_owned(),
                name: "fs_read".to_owned(),
                args: serde_json::json!({}),
                status: ToolStatus::Completed,
            },
        },
    ))));
    assert!(!model.animated(), "completed tool is still");

    // The processing todo's box pulses; a finished plan is still.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Started {
            item_id: ItemId::new("anim-plan"),
            item: TurnItem::Plan {
                items: vec![TodoItem {
                    id: 0,
                    text: "work".to_owned(),
                    state: TodoState::Processing,
                    dep: None,
                }],
            },
        },
    ))));
    assert!(model.animated(), "processing todo pulses");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Completed {
            item_id: ItemId::new("anim-plan"),
            item: TurnItem::Plan {
                items: vec![TodoItem {
                    id: 0,
                    text: "work".to_owned(),
                    state: TodoState::Completed,
                    dep: None,
                }],
            },
        },
    ))));
    assert!(!model.animated(), "a completed plan is still");

    // Chip states: running/input-required pulse; done is still.
    model.chips.push(seed_chip(ChipDisplayState::Running));
    assert!(model.animated(), "running chip glyph pulses");
    model.chips[0].state = ChipDisplayState::Done;
    assert!(!model.animated(), "done chip is still");
    model.chips[0].state = ChipDisplayState::InputRequired;
    assert!(model.animated(), "amber ? pulses");
    model.chips.clear();

    // The ◉ talk hold pulses wherever it is held.
    model.listening = true;
    assert!(model.animated(), "live talk hold pulses");
    model.listening = false;

    // The badge pulse set arms the clock on any screen.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::EffectOutcomeUnknown,
    ))));
    assert!(model.animated(), "⌁ EFFECT_UNKNOWN badge pulses");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));

    // Aura: a listening orb or a running roster row arms it.
    model.screen = Screen::Aura;
    model.aura.roster.clear();
    assert!(!model.animated(), "an idle aura is still");
    model.aura.state = AuraState::Listening;
    assert!(model.animated(), "aura hold-to-talk pulses");
    model.aura.state = AuraState::Idle;
    model.aura.roster.push(haider_tui::app::AuraAgentRow {
        name: "billing".to_owned(),
        device: "workstation".to_owned(),
        state: ChipDisplayState::Running,
        activity: "running tests".to_owned(),
    });
    assert!(model.animated(), "running roster row pulses");

    // Boot always breathes (the .sub line + current check).
    model.screen = Screen::Boot;
    assert!(model.animated(), "boot breathes");
}

// ---- The badge pulse set is the sim's, verbatim ----

#[test]
fn badge_pulse_set_matches_the_sim_vocabulary() {
    // MUTATION CHECK: add "⏸ IDLE" (IDLE_I) or "? INPUT_REQUIRED" to
    // `badge_pulses` and the still-set below fails — the sim's array
    // (tui.js:5559) holds exactly WAITING · STARTING · PERMISSION ·
    // EFFECT_UNKNOWN; the other outlined states are deliberately still.
    for label in [
        "◔ WAITING · provider backoff",
        "◔ WAITING · 2 subagents",
        "◌ STARTING",
        "? PERMISSION_REQUIRED",
        "⌁ EFFECT_UNKNOWN",
    ] {
        assert!(badge_pulses(label), "{label} pulses");
    }
    for label in [
        "IDLE",
        "⏸ IDLE (i)",
        "? INPUT_REQUIRED",
        "● THINKING",
        "▮ STREAMING",
        "⚒ TOOL_RUNNING",
        "⊟ COMPACTING",
        "⚙ VERIFYING · check",
        "◆ CONCLUDING",
        "⊘ CANCELLING",
        "✗ ERRORED",
        "◌ QUEUED",
    ] {
        assert!(!badge_pulses(label), "{label} is still");
    }
}

// ---- Phase ticks never reach the persistence layer ----

#[test]
fn phase_ticks_never_touch_the_persistence_snapshot() {
    // MUTATION CHECK: persist `anim_phase` in the demo store's snapshot
    // (add it to StateDto) and the two serializations below diverge — the
    // save-on-frame hash-skip would then WRITE on every phase tick, the
    // exact thrash the efficiency law forbids.
    let mut model = launcher_model();
    haider_tui::app::AppModel::handle(
        &mut model,
        AppEvent::Envelope(Box::new(EventPayload::RunState(RunState::Thinking))),
    );
    model.anim_phase = 0;
    let at_zero =
        serde_json::to_string(&haider_tui::demo_store::snapshot(&model)).expect("serialize");
    model.anim_phase = 201;
    let at_tick =
        serde_json::to_string(&haider_tui::demo_store::snapshot(&model)).expect("serialize");
    assert_eq!(
        at_zero, at_tick,
        "the snapshot covers persisted state only — phase ticks are hash-invisible"
    );
}

// ---- The launcher: dot pulse + rail shimmer, idle rows untouched ----

#[test]
fn the_launcher_rail_shimmers_and_the_running_dot_pulses() {
    // MUTATION CHECK: drop the rail span (or render the running dot from
    // the static gold style) and the ▎ symbol / phase-varying inks below
    // disappear.
    let mut model = launcher_model();
    let theme = model.theme.theme();
    let colors = |model: &AppModel| {
        let (rows, terminal) = draw(model, 118, 34);
        let y = row_of(&rows, "◉ l1-remote-projects");
        let dot_x = col_of(&rows[y as usize], "◉");
        let buffer = terminal.backend().buffer();
        let rail = buffer[(dot_x - 1, y)].clone();
        (rail.symbol().to_owned(), rail.fg, buffer[(dot_x, y)].fg)
    };
    model.anim_phase = 0;
    let (rail0, rail0_fg, dot0) = colors(&model);
    model.anim_phase = 1;
    let (rail1, rail1_fg, dot1) = colors(&model);
    model.anim_phase = 2;
    let (_, rail2_fg, dot2) = colors(&model);

    assert_eq!(rail0, "▎", "the running row carries the rail sliver");
    assert_eq!(rail1, "▎");
    assert_eq!(rail0_fg, Color::from(theme.gold), "shimmer phase 0: gold");
    assert_eq!(rail1_fg, Color::from(theme.maroon), "phase 1: maroon");
    assert_eq!(rail2_fg, Color::from(theme.gold), "phase 2: gold again");
    assert_eq!(dot0, Color::from(theme.gold), "dot ON phase");
    assert_eq!(
        dot1,
        Color::from(theme.gold.over(theme.bg, 350)),
        "dot OFF phase dims"
    );
    assert_eq!(dot0, dot2, "period two");

    // Idle rows pay the rail CELL (shared left edge) but never the glyph.
    let (rows, _) = draw(&model, 118, 34);
    let y = row_of(&rows, "● billing-service");
    let dot_x = col_of(&rows[y as usize], "●");
    let (rows_again, terminal) = draw(&model, 118, 34);
    assert_eq!(rows, rows_again);
    assert_eq!(
        terminal.backend().buffer()[(dot_x - 1, y)].symbol(),
        " ",
        "idle rows carry a blank rail cell"
    );
}

// ---- Chip glyphs pulse in their sim hues ----

#[test]
fn chip_glyphs_pulse_running_maroon_and_input_required_warn() {
    let mut model = launcher_model();
    common::hit_session_named(&mut model, "billing-service");
    model.chips.push(seed_chip(ChipDisplayState::Running));
    let theme = model.theme.theme();

    model.anim_phase = 0;
    let on = fg_at(&model, "◐ Salman");
    model.anim_phase = 1;
    let off = fg_at(&model, "◐ Salman");
    assert_eq!(on, Color::from(theme.maroon), "running chip: maroon ON");
    assert_eq!(off, Color::from(theme.maroon.over(theme.bg, 350)));

    model.chips[0].state = ChipDisplayState::InputRequired;
    model.anim_phase = 0;
    let on = fg_at(&model, "? Salman");
    model.anim_phase = 1;
    let off = fg_at(&model, "? Salman");
    assert_eq!(on, Color::from(theme.warn), "amber ?: warn ON");
    assert_eq!(off, Color::from(theme.warn.over(theme.bg, 350)));

    // A done chip is STILL: identical frames across phases.
    model.chips[0].state = ChipDisplayState::Done;
    model.anim_phase = 0;
    let (frame_a, _) = draw(&model, 118, 34);
    model.anim_phase = 1;
    let (frame_b, _) = draw(&model, 118, 34);
    assert_eq!(frame_a, frame_b, "nothing animated → phase-invariant frame");
}
