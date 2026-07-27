//! In-app drag selection (owner item 9).
//!
//! Mouse capture eats the terminal's native drag-select, so the TUI owns
//! selection itself: Down anchors, Drag extends a LINEAR terminal-style
//! range (anchor cell to head cell flowing across rows — first row from the
//! anchor column to the right edge, whole middle rows, last row from the
//! left edge to the head column), Up auto-copies. This replaces the old
//! "text selection is left to native ⇧-drag" ledger row — the owner asked
//! for the in-app behavior explicitly.
//!
//! The selection is SCREEN-space, not transcript-space: it highlights the
//! rendered cells and copies what is on screen, exactly like a terminal
//! emulator's selection of a full-screen app. Scrolling under a finished
//! selection therefore shifts content beneath the highlight; the next
//! click or keypress clears it (the same law terminals apply to a stale
//! selection: it is a snapshot, not an anchor into the document).

use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr;

/// A live or completed drag selection, in screen cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    /// Where the drag started (column, row).
    pub anchor: (u16, u16),
    /// Where the pointer is / ended (column, row), inclusive.
    pub head: (u16, u16),
    /// Still being dragged (button held). A completed selection keeps its
    /// highlight until the next click or keypress clears it.
    pub dragging: bool,
}

impl Selection {
    /// The range in reading order: `(start, end)` ordered by row, then
    /// column — the anchor may sit after the head when the user dragged
    /// upward or leftward.
    #[must_use]
    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        let a = (self.anchor.1, self.anchor.0);
        let h = (self.head.1, self.head.0);
        if a <= h {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The selected column span on `row` for a screen `width` cells wide:
    /// `Some(first..=last)` when the row intersects the linear range.
    #[must_use]
    pub fn row_span(&self, row: u16, width: u16) -> Option<(u16, u16)> {
        let ((sc, sr), (ec, er)) = self.normalized();
        if row < sr || row > er || width == 0 {
            return None;
        }
        let first = if row == sr { sc } else { 0 };
        let last = if row == er { ec } else { width - 1 };
        let last = last.min(width - 1);
        (first <= last).then_some((first, last))
    }
}

/// Extract the selected text from a rendered frame: per row, the covered
/// cells' symbols with trailing pad spaces trimmed; rows joined with
/// newlines. Wide glyphs count once — their continuation cells are skipped
/// by symbol width, never re-read as padding.
#[must_use]
pub fn selection_text(buffer: &Buffer, selection: &Selection) -> String {
    let area = buffer.area;
    let mut rows: Vec<String> = Vec::new();
    let ((_, sr), (_, er)) = selection.normalized();
    for row in sr..=er.min(area.height.saturating_sub(1)) {
        let Some((first, last)) = selection.row_span(row, area.width) else {
            continue;
        };
        let mut text = String::new();
        let mut x = first;
        while x <= last {
            let symbol = buffer[(area.x + x, area.y + row)].symbol();
            text.push_str(symbol);
            // A wide glyph's continuation cell holds filler — skip it so
            // "حيدر" or CJK never gains phantom spaces.
            x += u16::try_from(symbol.width().max(1)).unwrap_or(1);
        }
        rows.push(text.trim_end().to_owned());
    }
    rows.join("\n")
}

/// Paint the selection ground over the rendered frame — bg only, glyph inks
/// untouched (the hover-band law: sim `selBg` shifts the ground, never the
/// ink). Runs AFTER every widget so the highlight reads on any screen.
pub fn apply_highlight(buffer: &mut Buffer, selection: &Selection, theme: &crate::theme::Theme) {
    let area = buffer.area;
    let ((_, sr), (_, er)) = selection.normalized();
    for row in sr..=er.min(area.height.saturating_sub(1)) {
        let Some((first, last)) = selection.row_span(row, area.width) else {
            continue;
        };
        for x in first..=last {
            buffer[(area.x + x, area.y + row)].set_bg(theme.sel_bg.into());
        }
    }
}
