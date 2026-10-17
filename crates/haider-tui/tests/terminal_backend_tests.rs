#![allow(clippy::unwrap_used)]

use haider_tui::terminal_backend::CellBackend;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::Style;

#[test]
fn wide_grapheme_trailing_clears_precede_ink_and_next_cell_is_addressed() {
    for glyph in ["⚙️", "✏️", "🖥️", "🔍", "界", "👩‍💻", "🇮🇷"] {
        let before = Buffer::with_lines(["patch the parser"]);
        let mut after = Buffer::empty(before.area);
        after.set_string(0, 0, format!("{glyph} row"), Style::default());
        let mut bytes = Vec::new();
        CellBackend::new(&mut bytes)
            .draw(before.diff(&after).into_iter())
            .unwrap();
        let stream = String::from_utf8(bytes).unwrap();
        let ink = stream.find(glyph).unwrap();
        let next = format!("\x1b[1;{}H", glyph.cell_width() + 1);
        assert!(stream[ink..].contains(&next), "{glyph}: {stream:?}");
        // No cursor may return into the new glyph's continuation cells after its ink.
        for column in 2..=glyph.cell_width() {
            assert!(
                !stream[ink..].contains(&format!("\x1b[1;{column}H")),
                "{glyph}: {stream:?}"
            );
        }
    }
}

#[test]
fn single_cell_text_and_non_vs16_controls_keep_contiguous_draws() {
    for text in ["[run] row", "run row", "e\u{301} row"] {
        let before = Buffer::empty(Rect::new(0, 0, 20, 1));
        let mut after = before.clone();
        after.set_string(0, 0, text, Style::default());
        let mut bytes = Vec::new();
        CellBackend::new(&mut bytes)
            .draw(before.diff(&after).into_iter())
            .unwrap();
        let mut original = Vec::new();
        CrosstermBackend::new(&mut original)
            .draw(before.diff(&after).into_iter())
            .unwrap();
        assert_eq!(bytes, original, "single-cell control {text}");
    }
}
