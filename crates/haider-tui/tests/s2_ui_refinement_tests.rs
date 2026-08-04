//! S2 UI refinements (owner screenshots wave): the rebuilt 24×2 header
//! mark, the composer band's one-line rest height, the transcript/composer
//! breathing rhythm, and the child chip view's user rows.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, Hit};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::theme::ThemeKey;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

mod common;
use common::{launcher_model, submit};

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

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
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, hits, terminal)
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row containing {needle:?} not rendered")),
    )
    .expect("row fits u16")
}

// ---- Owner item 3: the rebuilt header mark ----

/// MUTATION CHECK: regress `mark::HEADER` to the 16-col block map (or any
/// map that renders different glyph rows) and this fails on the verbatim
/// row pins — the owner's screenshot showed the 16-col mark reading as
/// disconnected squares, so the exact rebuilt art is the contract.
#[test]
fn header_mark_uses_halfblock_glyphs() {
    // The rendered rows, VERBATIM (24 cols × 2 rows): the banner's
    // letterforms at half vertical resolution — `ر`'s body sweeping into
    // its tail, `ـد`'s solid upright, `ـيـ`'s tooth with both dots
    // hanging beneath the `▀` baseline, `حـ`'s head bar thickening into
    // the drop at the right edge.
    let rows = haider_tui::mark::header_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], "   ▄▄  ██   ▄▄  ▀▀▀▀▀██▄");
    assert_eq!(rows[1], "▄▄█▀  ▀▀▀▀██▀▀██▀▀▀▀▀▀▀▀");
    // Half-block ink only — the doubled vertical resolution is the point.
    for row in &rows {
        assert!(row.chars().all(|c| "█▀▄ ".contains(c)));
        assert!(
            row.chars().any(|c| "█▀▄".contains(c)),
            "every terminal row carries ink"
        );
    }
    // Width/floor pins travel with the art.
    assert_eq!(haider_tui::mark::HEADER_COLS, 24);
    assert_eq!(haider_tui::mark::HEADER_ROWS, 2);
}

/// The mark must look intentional in BOTH redesigned palettes (owner
/// item 3): the session header renders the full art in bold identity ink
/// on light and dark alike.
#[test]
fn header_mark_renders_in_both_new_palettes() {
    let art = haider_tui::mark::header_rows();
    for key in [ThemeKey::Light, ThemeKey::Dark] {
        let mut model = session_model();
        model.theme = key;
        let theme = key.theme();
        let (rows, _, terminal) = draw(&model, 118, 34);
        let buffer = terminal.backend().buffer();
        assert!(
            rows[0].contains(art[0].trim_end()),
            "{}: mark line 1",
            theme.label
        );
        assert!(
            rows[1].contains(art[1].trim_end()),
            "{}: mark line 2",
            theme.label
        );
        // Cell coordinates are CHAR columns, never byte offsets (the art
        // is multi-byte ink).
        let char_x = u16::try_from(
            rows[0]
                .chars()
                .position(|c| c == '█')
                .expect("mark ink cell"),
        )
        .expect("x fits");
        let cell = &buffer[(char_x, 0)];
        assert_eq!(cell.fg, Color::from(theme.maroon), "{}: identity ink", theme.label);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }
}
