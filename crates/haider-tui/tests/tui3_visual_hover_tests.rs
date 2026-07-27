//! TUI3a owner-parity guards: exact-RGB visual assertions (composer band,
//! launcher typography, status transparency) and mouse hover through the
//! production dispatch (rows light up, palette/menu selection follows,
//! motion floods never dirty without a change).
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_tui::app::{AppEvent, AppModel, Hit};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
use haider_tui::theme::{Rgb, ThemeKey};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
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

fn moved(column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row containing {needle:?} not rendered")),
    )
    .expect("row fits u16")
}

fn col_of(row: &str, needle: &str) -> u16 {
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("column of {needle:?} not found in row {row:?}"));
    u16::try_from(row[..byte].chars().count()).expect("col fits u16")
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

// ---- Item 1: the composer band, exact white-blend, no stray tints ----

#[test]
fn composer_band_is_the_exact_sim_blend_in_all_three_themes() {
    // Hand-computed sim blends (inputBg over bg, round half-up):
    //   dawn : white 28.0% over #f3ead9 → (246, 240, 228)
    //   ivory: ink   3.5% over #fbf9f3 → (244, 241, 235)
    //   dark : white  4.5% over #14100a → ( 31,  27,  21)
    let expected = [
        (
            ThemeKey::Dawn,
            Rgb {
                r: 246,
                g: 240,
                b: 228,
            },
        ),
        (
            ThemeKey::Ivory,
            Rgb {
                r: 244,
                g: 241,
                b: 235,
            },
        ),
        (
            ThemeKey::Dark,
            Rgb {
                r: 31,
                g: 27,
                b: 21,
            },
        ),
    ];
    for (theme_key, rgb) in expected {
        assert_eq!(
            theme_key.theme().input_bg,
            rgb,
            "{} inputBg blend",
            theme_key.theme().label
        );
        let mut model = session_model();
        model.theme = theme_key;
        let theme = theme_key.theme();
        let (rows, _, terminal) = draw(&model, 118, 34);
        let buffer = terminal.backend().buffer();
        let composer_y = row_of(&rows, "message haider");
        // The band itself: exact blend, LIGHTER-than-page white blend on
        // dawn (never the tan bar tint).
        assert_eq!(buffer[(0, composer_y)].bg, Color::from(rgb));
        // Exactly one gold rule above, plain ground below (gap) and on the
        // status row — no stray tinted rows bracket the band.
        assert_eq!(buffer[(0, composer_y - 1)].fg, Color::from(theme.gold));
        assert_eq!(buffer[(0, composer_y - 1)].bg, Color::from(theme.bg));
        // TUI4 item 2: the row BELOW the composer is the band's padding row
        // — the band is one complete panel, not a stripe behind the text —
        // and the row after that is its closing frame rule on plain ground.
        assert_eq!(buffer[(0, composer_y + 1)].bg, Color::from(rgb));
        assert_eq!(buffer[(0, composer_y + 2)].bg, Color::from(theme.bg));
        let status_y = u16::try_from(rows.len() - 1).expect("status row");
        assert_eq!(
            buffer[(0, status_y)].bg,
            Color::from(theme.bg),
            "status bar ground is transparent (the old bar tint was the tan band)"
        );
        // Gold BOLD sigil.
        let sigil_x = col_of(&rows[composer_y as usize], "❯");
        let sigil = &buffer[(sigil_x, composer_y)];
        assert_eq!(sigil.fg, Color::from(theme.gold));
        assert!(sigil.modifier.contains(Modifier::BOLD));
        // Talk chip: FRAME chrome, gold label (sim .mic).
        let chip_x = col_of(&rows[composer_y as usize], "[ ◉ talk ]");
        assert_eq!(buffer[(chip_x, composer_y)].fg, Color::from(theme.frame));
        let label_x = col_of(&rows[composer_y as usize], "◉ talk");
        assert_eq!(buffer[(label_x, composer_y)].fg, Color::from(theme.gold));
    }
}

// ---- Item 2: the launcher .recent column ----

#[test]
fn launcher_recent_rows_share_one_left_column_without_digits() {
    let model = launcher_model();
    let (rows, _, _) = draw(&model, 118, 34);
    // Sim rhead verbatim + gold running count.
    let head_y = row_of(
        &rows,
        "recent sessions — click to attach · /sessions for all",
    );
    assert!(rows[head_y as usize].contains("· 1 running"));
    // No digit prefixes.
    assert!(!rows.iter().any(|row| row.contains("1 ●")));
    // Every row of the block starts at the SAME left column.
    let left = col_of(&rows[head_y as usize], "recent sessions");
    for needle in [
        // TUI3.1 P2-8: liveness follows the SIM's seeds — the L1 session
        // owns the running `web-index` chip, so it is the one live row.
        "● billing-service",
        "● cellular-pool-fix",
        "◉ l1-remote-projects",
        "◉ Aura",
        "⚿ Accounts",
        "⇄ Peers",
    ] {
        let y = row_of(&rows, needle);
        assert_eq!(
            col_of(
                &rows[y as usize],
                needle
                    .chars()
                    .next()
                    .map(|c| &needle[..c.len_utf8()])
                    .expect("head char")
            ),
            left,
            "{needle} aligned to the shared column"
        );
    }
    // A frame rule sits directly above EACH of the three aura rows.
    let theme = model.theme.theme();
    let (_, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    for needle in ["◉ Aura", "⚿ Accounts", "⇄ Peers"] {
        let y = row_of(&rows, needle);
        let rule_row = &rows[(y - 1) as usize];
        let rule_x = col_of(rule_row, "─");
        // TUI4d item 14: every block row leads with the one-cell rail
        // column (the sim's `.rail` sliver floats in the rows' left
        // padding, tui.js:4370-4394), so the rule spans rail + text —
        // one column to the left of the shared TEXT edge.
        assert_eq!(rule_x, left - 1, "aurarow rule spans rail + text");
        assert_eq!(buffer[(rule_x, y - 1)].fg, Color::from(theme.frame));
    }
}

#[test]
fn launcher_typography_uses_exact_theme_inks() {
    let model = launcher_model();
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    // Mark: BOLD maroon (#7c2d12 on dawn).
    // TUI4 item 4: the mark is half-block art at this width.
    let mark_y = row_of(&rows, &haider_tui::mark::banner_rows()[2]);
    let mark_x = col_of(&rows[mark_y as usize], "█");
    let mark = &buffer[(mark_x, mark_y)];
    assert_eq!(mark.fg, Color::Rgb(0x7c, 0x2d, 0x12));
    assert!(mark.modifier.contains(Modifier::BOLD));
    // Shahada: gold (#9a6a08 on dawn), NOT bold.
    let shahada_y = row_of(&rows, "لا إله إلا الله");
    let shahada_x = col_of(&rows[shahada_y as usize], "لا");
    let shahada = &buffer[(shahada_x, shahada_y)];
    assert_eq!(shahada.fg, Color::Rgb(0x9a, 0x6a, 0x08));
    assert!(!shahada.modifier.contains(Modifier::BOLD));
    // The dignity rule: gold at HALF strength over the ground.
    let rule_y = row_of(&rows, "────────────────");
    let rule_x = col_of(&rows[rule_y as usize], "─");
    assert_eq!(buffer[(rule_x, rule_y)].fg, Color::from(theme.gold_half));
    // Wordmark bright.
    let word_y = row_of(&rows, "H A I D E R");
    let word_x = col_of(&rows[word_y as usize], "H");
    assert_eq!(buffer[(word_x, word_y)].fg, Color::Rgb(0x3d, 0x2d, 0x18));
    // Info line: labels DIM, values BRIGHT.
    let info_y = row_of(&rows, "provider anthropic");
    let label_x = col_of(&rows[info_y as usize], "provider");
    let value_x = col_of(&rows[info_y as usize], "anthropic");
    assert_eq!(buffer[(label_x, info_y)].fg, Color::from(theme.dim));
    assert_eq!(buffer[(value_x, info_y)].fg, Color::from(theme.bright));
    // Aura row name: gold.
    let aura_y = row_of(&rows, "◉ Aura");
    let aura_x = col_of(&rows[aura_y as usize], "Aura");
    assert_eq!(buffer[(aura_x, aura_y)].fg, Color::from(theme.gold));
}

// ---- Item 4: SessHead l1 dir bright ----

#[test]
fn session_header_dir_is_bright_and_back_chip_frame_chromed() {
    let model = session_model();
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let l1_y = row_of(&rows, "haider v");
    // l1 base is BRIGHT: the ` · dir` segment.
    let dir_x = col_of(&rows[l1_y as usize], "· ~");
    assert_eq!(buffer[(dir_x, l1_y)].fg, Color::from(theme.bright));
    // Back chip chrome: FRAME brackets, dim label (sim .backbtn).
    let chip_x = col_of(&rows[l1_y as usize], "[ ← main ]");
    assert_eq!(buffer[(chip_x, l1_y)].fg, Color::from(theme.frame));
    let label_x = col_of(&rows[l1_y as usize], "← main");
    assert_eq!(buffer[(label_x, l1_y)].fg, Color::from(theme.dim));
}

// ---- Item 5: boot verbatim texts ----

#[test]
fn boot_plays_the_sim_script_verbatim() {
    let mut model = AppModel::new();
    for payload in demo_script().into_iter().take(3) {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let (rows, _, _) = draw(&model, 118, 34);
    for needle in [
        "store open · journal replayed",
        "provider handshake — anthropic ✓",
        "hooks loaded — 1 trusted · 0 pending",
        "worker warm · mesh probe done",
    ] {
        assert!(
            rows.iter().any(|row| row.contains(needle)),
            "sim boot line verbatim: {needle}"
        );
    }
}

// ---- Item 6: hover through production dispatch ----

fn rect_center(rect: Rect) -> (u16, u16) {
    (rect.x + rect.width / 2, rect.y)
}

#[test]
fn hover_lights_launcher_rows_and_clears_off_target() {
    let mut model = launcher_model();
    let (rows, hits, _) = draw(&model, 118, 34);
    let (rect, _) = hits
        .iter()
        .find(|(_, h)| *h == Hit::AttachSample("billing-service".to_owned()))
        .expect("sample row hit");
    let row_y = rect.y;
    let (cx, cy) = rect_center(*rect);
    dispatch_input(&mut model, &hits, moved(cx, cy));
    assert_eq!(
        model.hovered,
        Some(Hit::AttachSample("billing-service".to_owned()))
    );
    assert!(model.dirty, "hover change dirties");
    let theme = model.theme.theme();
    let (_, _, terminal) = draw(&model, 118, 34);
    let left = col_of(&rows[row_y as usize], "●");
    assert_eq!(
        terminal.backend().buffer()[(left, row_y)].bg,
        Color::from(theme.sel_bg),
        "hovered row wears the selBg band"
    );
    // Motion flood on the SAME target never re-dirties.
    model.dirty = false;
    dispatch_input(&mut model, &hits, moved(cx, cy));
    assert!(!model.dirty, "no dirty without a hover change");
    // Moving off every region clears the hover.
    dispatch_input(&mut model, &hits, moved(0, 0));
    assert_eq!(model.hovered, None);
    assert!(model.dirty);
    let (_, _, terminal) = draw(&model, 118, 34);
    assert_ne!(
        terminal.backend().buffer()[(left, row_y)].bg,
        Color::from(theme.sel_bg),
        "band gone once the pointer leaves"
    );
}

#[test]
fn hover_moves_palette_and_menu_selection() {
    // Palette: hover selects (sim onMouseEnter, tui.js:2992).
    let mut model = session_model();
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 118, 34);
    let tree_rect = hits
        .iter()
        .find_map(|(rect, h)| match h {
            Hit::PaletteRow(item) if item.label() == "/tree" => Some(*rect),
            _ => None,
        })
        .expect("tree row");
    let (cx, cy) = rect_center(tree_rect);
    dispatch_input(&mut model, &hits, moved(cx, cy));
    assert_eq!(model.palette_selection, 1, "hover moved the selection");

    // Menu options: hover selects (tui.js:3073).
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let (_, hits, _) = draw(&model, 118, 34);
    let deny_rect = hits
        .iter()
        .find_map(|(rect, h)| match h {
            Hit::MenuOption { index: 1, .. } => Some(*rect),
            _ => None,
        })
        .expect("deny row");
    let (cx, cy) = rect_center(deny_rect);
    dispatch_input(&mut model, &hits, moved(cx, cy));
    assert_eq!(model.menu_selection, 1, "hover moved the menu selection");
}

#[test]
fn hover_turns_talk_and_back_chip_chrome_gold() {
    let mut model = session_model();
    let (rows, hits, _) = draw(&model, 118, 34);
    let theme = model.theme.theme();
    // Talk chip hover: chrome frame → gold.
    let (talk_rect, _) = hits
        .iter()
        .find(|(_, h)| *h == Hit::TalkChip)
        .expect("talk chip");
    let composer_y = talk_rect.y;
    let chip_x = col_of(&rows[composer_y as usize], "[ ◉ talk ]");
    let (cx, cy) = rect_center(*talk_rect);
    dispatch_input(&mut model, &hits, moved(cx, cy));
    let (_, _, terminal) = draw(&model, 118, 34);
    assert_eq!(
        terminal.backend().buffer()[(chip_x, composer_y)].fg,
        Color::from(theme.gold),
        "hovered .mic border goes gold"
    );
    // Back chip hover: text + border gold.
    let (back_rect, _) = hits
        .iter()
        .find(|(_, h)| *h == Hit::BackChip)
        .expect("back chip");
    let (cx, cy) = rect_center(*back_rect);
    dispatch_input(&mut model, &hits, moved(cx, cy));
    let (rows, _, terminal) = draw(&model, 118, 34);
    let label_x = col_of(&rows[back_rect.y as usize], "← main");
    assert_eq!(
        terminal.backend().buffer()[(label_x, back_rect.y)].fg,
        Color::from(theme.gold),
        "hovered .backbtn goes gold"
    );
}
