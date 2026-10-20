//! Width-safe drawing over the locked Crossterm backend.
//!
//! Its adjacent-cell optimization assumes every printed symbol advances one
//! column. Ratatui's diff can also clear a VS16 glyph's trailing cells AFTER
//! printing it. Clear those cells first, then draw the glyph and start a fresh
//! cursor-addressed batch. Normal single-cell runs retain the fast path.

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellWidth};
use ratatui::layout::{Position, Size};
use std::io::{self, Write};

pub struct CellBackend<W: Write> {
    inner: CrosstermBackend<W>,
}

impl<W: Write> CellBackend<W> {
    pub const fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
        }
    }
}

impl<W: Write> Write for CellBackend<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<W: Write> Backend for CellBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut cells = content.peekable();
        let mut narrow = Vec::new();
        while let Some((x, y, cell)) = cells.next() {
            let width = cell.cell_width();
            if width == 1 {
                narrow.push((x, y, cell));
                continue;
            }
            if !narrow.is_empty() {
                self.inner.draw(narrow.drain(..))?;
            }
            // Trailing writes overlap this glyph. Draw them BEFORE its ink,
            // preserving background/style clears without erasing half a glyph.
            let end = x.saturating_add(width);
            let mut trailing = Vec::new();
            while let Some(cell) = cells.next_if(|(tx, ty, _)| *ty == y && *tx > x && *tx < end) {
                trailing.push(cell);
            }
            if !trailing.is_empty() {
                self.inner.draw(trailing.into_iter())?;
            }
            self.inner.draw(std::iter::once((x, y, cell)))?;
            // A new draw starts with MoveTo; no assumption about a wide,
            // combining, VS16 or ZWJ symbol's terminal cursor advance survives.
        }
        if !narrow.is_empty() {
            self.inner.draw(narrow.into_iter())?;
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> io::Result<()> {
        self.inner.set_cursor_position(p)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, t: ClearType) -> io::Result<()> {
        self.inner.clear_region(t)
    }
    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}
