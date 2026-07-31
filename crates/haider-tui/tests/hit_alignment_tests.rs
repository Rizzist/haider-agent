//! Mouse-parity guards: every clickable region's hit RECT must align with
//! the row content actually RENDERED at that frame size, and the composer /
//! palette must carry the sim's exact visual signature (gold rule, inputBg
//! ground, bold gold sigil, maroon→gold palette names, bottom hints).
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_tui::app::{AppEvent, AppModel, Hit, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::theme::ThemeKey;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

mod common;
use common::{key, launcher_model};

fn draw(
    model: &AppModel,
    width: u16,
    height: u16,
) -> (Vec<String>, Vec<(Rect, Hit)>, Terminal<TestBackend>) {
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
    (rows, hits, terminal)
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
fn rect_for(hits: &[(Rect, Hit)], hit: Hit) -> Rect {
    let mut matches = hits.iter().filter(|(_, h)| *h == hit);
    let (rect, _) = matches
        .next()
        .unwrap_or_else(|| panic!("no hit region for {hit:?}"));
    assert!(
        matches.next().is_none(),
        "duplicate hit regions for {hit:?}"
    );
    *rect
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn menu_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

#[test]
fn launcher_row_hits_align_with_their_rendered_rows() {
    let model = launcher_model();
    let (rows, hits, _) = draw(&model, 118, 34);
    for name in ["billing-service", "cellular-pool-fix", "l1-remote-projects"] {
        let rect = rect_for(
            &hits,
            Hit::AttachSession(common::session_named(&model, name)),
        );
        assert_eq!(rect.y, row_of(&rows, name), "sample row {name} aligned");
        assert_eq!(rect.height, 1);
    }
    for (row, name) in [
        (haider_tui::app::LauncherRow::Aura, "Aura"),
        (haider_tui::app::LauncherRow::Accounts, "Accounts"),
        (haider_tui::app::LauncherRow::Peers, "Peers"),
    ] {
        let rect = rect_for(&hits, Hit::ExtraRow(row));
        assert_eq!(rect.y, row_of(&rows, name), "extra row {name} aligned");
    }
}

#[test]
fn talk_chip_hit_covers_exactly_the_rendered_chip() {
    let model = launcher_model();
    let (rows, hits, _) = draw(&model, 118, 34);
    let composer_y = row_of(&rows, "start a session");
    let rect = rect_for(&hits, Hit::TalkChip);
    assert_eq!(rect.y, composer_y, "chip sits on the composer row");
    let chip_col = col_of(&rows[composer_y as usize], "[ ◉ talk ]");
    assert_eq!(rect.x, chip_col, "hit starts at the chip's [");
    assert_eq!(rect.width, 10, "hit spans exactly [ ◉ talk ]");
    // The launcher mic RENDERS but is inert — sim `speak` returns unless a
    // session is attached (tui.js:2045, review r2 P2-3).
    let mut model = model;
    model.handle_hit(Hit::TalkChip);
    assert!(!model.listening, "no session → no hold");
    assert!(model.requests.is_empty(), "and no driver timer");

    // Inside an idle session the same chip starts the 1300 ms hold (TUI3b
    // §4.1 — voice ships ON): it flips to `◉ listening…`, driver-timed.
    for c in "walk the harness".chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    model.requests.clear();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        haider_protocol::state::RunState::Done,
    ))));
    assert_eq!(model.screen, Screen::Session);
    assert!(!model.turn_active, "an idle session");
    model.handle_hit(Hit::TalkChip);
    assert!(model.listening, "hold started");
    assert!(
        model.requests.contains(&haider_tui::app::AppRequest::Talk),
        "driver timer requested"
    );
    let (rows, _, _) = draw(&model, 118, 34);
    assert!(
        rows.iter().any(|row| row.contains("[ ◉ listening… ]")),
        "chip shows the live hold"
    );
}

#[test]
fn help_hint_hit_covers_the_rendered_hint_text() {
    let model = launcher_model();
    let (rows, hits, _) = draw(&model, 118, 34);
    let status_y = u16::try_from(rows.len() - 1).expect("status row");
    let hint_col = col_of(&rows[status_y as usize], "/help · theme");
    let rect = rect_for(&hits, Hit::HelpHint);
    assert_eq!(rect.y, status_y);
    assert!(
        hint_col >= rect.x && hint_col < rect.x + rect.width,
        "hint text starts inside the hit region"
    );
}

#[test]
fn back_chip_hit_covers_the_rendered_chip() {
    let model = session_model();
    let (rows, hits, _) = draw(&model, 118, 34);
    let rect = rect_for(&hits, Hit::BackChip);
    assert_eq!(rect.y, row_of(&rows, "← main"));
    let row = &rows[rect.y as usize];
    let chip_col = col_of(row, "[ ← main ]");
    assert!(
        chip_col >= rect.x && chip_col < rect.x + rect.width,
        "chip text inside the hit region"
    );
}

#[test]
fn palette_row_hits_align_and_click_runs_that_row() {
    let mut model = session_model();
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // Session matches for /t, sim registry order: theme · tree · tokens · tools.
    let items = model.palette_items();
    let names: Vec<String> = items.iter().map(|s| s.label()).collect();
    assert_eq!(names, ["/theme", "/tree", "/tokens", "/tools"]);
    let (rows, hits, _) = draw(&model, 118, 34);
    for (item, label) in items.iter().zip(["/theme", "/tree", "/tokens", "/tools"]) {
        let rect = rect_for(&hits, Hit::PaletteRow(item.clone()));
        assert_eq!(rect.y, row_of(&rows, label), "palette row {label} aligned");
    }
    assert_eq!(
        hits.iter()
            .filter(|(_, h)| matches!(h, Hit::PaletteRow(_)))
            .count(),
        4,
        "no hit region beyond the rendered rows"
    );
    // The bottom hint line is not clickable.
    let hint_y = row_of(&rows, "↑↓ options · tab complete · ⏎ run · esc dismiss");
    assert!(
        !hits
            .iter()
            .any(|(rect, h)| matches!(h, Hit::PaletteRow(_)) && rect.y == hint_y),
        "hint row carries no palette hit"
    );
    // Clicking the /tree row runs /tree (honest wave flash) — the hit
    // carries the VALUE it was rendered with.
    model.handle_hit(Hit::PaletteRow(items[1].clone()));
    assert!(model.flash.as_deref().unwrap_or("").contains("/tree"));
}

#[test]
fn menu_option_hits_align_with_their_rows() {
    use haider_protocol::ids::MenuId;
    let model = menu_model();
    let (rows, hits, _) = draw(&model, 90, 26);
    // Hits are bound to the menu they were rendered for (P2-2).
    let menu_id = MenuId::new("t0-menu-1");
    let allow = rect_for(
        &hits,
        Hit::MenuOption {
            menu: menu_id.clone(),
            index: 0,
        },
    );
    assert_eq!(allow.y, row_of(&rows, "1. Allow once"));
    let deny = rect_for(
        &hits,
        Hit::MenuOption {
            menu: menu_id,
            index: 1,
        },
    );
    assert_eq!(deny.y, row_of(&rows, "2. Deny"));
    // Body context lines render dim between title and options (P2-8).
    let body_y = row_of(&rows, "fs_patch wants to modify");
    assert!(body_y < allow.y, "body sits above the options");
    // The bottom hint names the menu id and the RPC answer contract.
    let hint_y = row_of(&rows, "menu.answer");
    assert!(rows[hint_y as usize].contains("menu t0-menu-1"));
    assert!(
        !hits
            .iter()
            .any(|(rect, h)| matches!(h, Hit::MenuOption { .. }) && rect.y == hint_y),
        "hint row carries no option hit"
    );
}

#[test]
fn composer_carries_the_sim_signature() {
    let model = session_model();
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.theme, ThemeKey::Dawn);
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    // Sim placeholder copy, verbatim (InputBar textarea, session variant).
    let composer_y = row_of(
        &rows,
        "message haider — ⏎ send · ⇧⏎ newline · / commands · paste images/text",
    );
    let buffer = terminal.backend().buffer();
    // The rule directly ABOVE the composer is GOLD (sim border-top: gold).
    let rule_y = composer_y - 1;
    assert_eq!(buffer[(0, rule_y)].symbol(), "─");
    assert_eq!(buffer[(0, rule_y)].fg, Color::from(theme.gold));
    // The composer row sits on the inputBg ground.
    assert_eq!(buffer[(0, composer_y)].bg, Color::from(theme.input_bg));
    // Bold gold ❯ sigil, 2-col padding off the frame edge.
    let sigil_x = col_of(&rows[composer_y as usize], "❯");
    assert_eq!(sigil_x, 2, "composer padded off the edge");
    let sigil = &buffer[(sigil_x, composer_y)];
    assert_eq!(sigil.fg, Color::from(theme.gold));
    assert!(sigil.modifier.contains(Modifier::BOLD), "sigil is bold");
    // Placeholder ink is the dim token (sim placeholder: dim @ 0.8).
    let ph_x = col_of(&rows[composer_y as usize], "message haider");
    assert_eq!(buffer[(ph_x, composer_y)].fg, Color::from(theme.dim));
    // Typed text renders bright with the CURSOR CELL after it. Directed
    // change (TUI5 item 1, owner law): this assertion used to pin the
    // appended `▮` glyph — the very bug the owner reported ("half into
    // the ground"). The cursor is now a styled CELL: gold ground,
    // badge_fg ink, a block over a space at end-of-text.
    // MUTATION CHECK (cursor-is-styled-cell-not-appended-glyph): revert
    // composer_cursor_row_spans to appending "▮" and this fails on the
    // no-▮ assertion AND the cell-style assertions.
    let mut model = model;
    for c in "hi".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let typed_y = row_of(&rows, "❯ hi");
    assert!(
        !rows[typed_y as usize].contains('▮'),
        "the composer cursor is a styled cell, never an appended ▮ glyph"
    );
    let typed_x = col_of(&rows[typed_y as usize], "hi");
    assert_eq!(buffer[(typed_x, typed_y)].fg, Color::from(theme.bright));
    let cursor_cell = &buffer[(typed_x + 2, typed_y)];
    assert_eq!(
        cursor_cell.symbol(),
        " ",
        "end-of-text cursor is a block over a space"
    );
    assert_eq!(
        cursor_cell.bg,
        Color::from(theme.gold),
        "cursor cell ground is gold"
    );
    assert_eq!(
        cursor_cell.fg,
        Color::from(theme.badge_fg),
        "cursor cell ink is the badge_fg reverse-video contrast"
    );
}

#[test]
fn palette_carries_the_sim_signature() {
    let mut model = session_model();
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let first_y = row_of(&rows, "/theme");
    // A frame rule tops the palette (sim CmdMenu border-top: frame).
    assert_eq!(buffer[(0, first_y - 1)].symbol(), "─");
    assert_eq!(buffer[(0, first_y - 1)].fg, Color::from(theme.frame));
    // Selected name GOLD on the selection ground; unselected names maroon.
    let name_x = col_of(&rows[first_y as usize], "/theme");
    assert_eq!(name_x, 2, "rows padded off the edge");
    assert_eq!(buffer[(name_x, first_y)].fg, Color::from(theme.gold));
    assert_eq!(buffer[(0, first_y)].bg, Color::from(theme.sel_bg));
    let second_y = row_of(&rows, "/tree");
    assert_eq!(buffer[(name_x, second_y)].fg, Color::from(theme.maroon));
    // Descriptions are dim, in the fixed column past the name gutter.
    let desc_x = col_of(&rows[second_y as usize], "Open the session tree");
    assert_eq!(desc_x, 18, "fixed-width name column");
    assert_eq!(buffer[(desc_x, second_y)].fg, Color::from(theme.dim));
    // The hint is the LAST palette line, directly above the composer rule.
    let hint_y = row_of(&rows, "↑↓ options");
    assert_eq!(
        hint_y,
        first_y + 4,
        "hint sits at the bottom of the four rows"
    );
}

#[test]
fn empty_palette_renders_nothing_and_esc_dismisses_keeping_text() {
    let mut model = launcher_model();
    for c in "/zzz".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(model.palette_items().is_empty());
    let (rows, hits, _) = draw(&model, 118, 34);
    assert!(
        !rows.iter().any(|row| row.contains("↑↓ options")),
        "no palette chrome without matches (sim hides the menu)"
    );
    assert!(!hits.iter().any(|(_, h)| matches!(h, Hit::PaletteRow(_))));

    // Esc dismisses the palette but KEEPS the typed text (sim menuDismissed).
    let mut model = launcher_model();
    for c in "/th".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(model.palette_open());
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.composer, "/th", "esc keeps the composer text");
    assert!(!model.palette_open(), "palette dismissed");
    // The next composer edit re-opens it.
    model.handle(key(KeyCode::Char('e')));
    assert!(model.palette_open(), "typing re-opens the palette");
}

#[test]
fn launcher_composer_is_bottom_anchored_with_the_gold_rule() {
    let model = launcher_model();
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let composer_y = row_of(&rows, "start a session");
    // Sim launcher: the InputBar sits at the bottom, above the status bar,
    // with its gold border-top; a spacer row keeps it off the bar itself.
    let height = u16::try_from(rows.len()).expect("height");
    assert_eq!(composer_y, height - 3, "composer anchored above the bar");
    assert_eq!(buffer[(0, composer_y - 1)].fg, Color::from(theme.gold));
    assert_eq!(buffer[(0, composer_y)].bg, Color::from(theme.input_bg));
    // The launcher shows dir + mesh (sim .dirline) — the dir is the
    // launcher's shell working dir (TUI3b §4: `cd` retargets it).
    assert!(
        rows.iter()
            .any(|row| row.contains("dir ~/dev/enterprise-suite · mesh off"))
    );
    // Sample metadata carries the blurb and branch count; TUI4 item 5 caps
    // the column at 70 cells, so the tail ellipsizes INTO the column rather
    // than running the frame's full width.
    assert!(
        rows.iter()
            .any(|row| row.contains("“Stripe webhooks + invoice backfill”"))
    );
    for needle in ["billing-service", "◉ Aura", "recent sessions"] {
        let row = rows
            .iter()
            .find(|row| row.contains(needle))
            .expect("column row");
        let start = row.chars().take_while(|c| *c == ' ').count();
        assert!(
            row.trim_end().chars().count().saturating_sub(start) <= 70,
            "{needle:?} exceeds the capped column"
        );
    }
    let (wide_rows, _, _) = draw(&model, 170, 34);
    assert!(
        wide_rows.iter().any(|row| row.contains("billing-service")),
        "full meta (model · device · ago) appears when the width allows"
    );
}

#[test]
fn thinking_beat_shows_the_transient_transcript_tail() {
    use haider_protocol::state::RunState;
    let mut model = AppModel::new();
    for payload in demo_script() {
        let is_thinking = matches!(payload, EventPayload::RunState(RunState::Thinking));
        model.handle(AppEvent::Envelope(Box::new(payload)));
        if is_thinking {
            break;
        }
    }
    assert!(model.projection.is_thinking());
    let (rows, _, _) = draw(&model, 118, 34);
    assert!(
        rows.iter().any(|row| row.contains("● thinking…")),
        "sim .thinking tail rendered during the THINKING beat"
    );
    // It is transient: gone once the run moves on.
    let mut model = session_model();
    assert!(!model.projection.is_thinking());
    let (rows, _, _) = draw(&model, 118, 34);
    assert!(!rows.iter().any(|row| row.contains("● thinking…")));
    // Typing is unaffected (the tail lives in the transcript, not the composer).
    model.handle(key(KeyCode::Char('x')));
    assert_eq!(model.composer, "x");
}

/// MUTATION CHECK (W5g-7): drop `compact_shift` from the launcher's
/// `visible` conversion (subtract only `dropped`). Expected runtime
/// failure: on a 24-row terminal — where the 4-row banner compacts to
/// one line — every launcher hit rect sits exactly 3 rows below its
/// painted row, the owner's "hover is off on the main menu".
#[test]
fn launcher_row_hits_align_on_the_compact_banner_path() {
    let model = launcher_model();
    let (rows, hits, _) = draw(&model, 118, 24);
    for name in ["billing-service", "cellular-pool-fix", "l1-remote-projects"] {
        let rect = rect_for(
            &hits,
            Hit::AttachSession(common::session_named(&model, name)),
        );
        assert_eq!(
            rect.y,
            row_of(&rows, name),
            "compact-path sample row {name} aligned"
        );
    }
    for (row, name) in [
        (haider_tui::app::LauncherRow::Aura, "Aura"),
        (haider_tui::app::LauncherRow::Accounts, "Accounts"),
        (haider_tui::app::LauncherRow::Peers, "Peers"),
    ] {
        let rect = rect_for(&hits, Hit::ExtraRow(row));
        assert_eq!(
            rect.y,
            row_of(&rows, name),
            "compact-path extra row {name} aligned"
        );
    }
}
