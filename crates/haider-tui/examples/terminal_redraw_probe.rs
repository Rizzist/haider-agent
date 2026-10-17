//! Real-PTY redraw fixture for scripts/qa-gate/activity-redraw-pty.py.
use haider_tui::terminal_backend::CellBackend;
use ratatui::{Terminal, widgets::Paragraph};
use std::{io, thread, time::Duration};

fn main() -> io::Result<()> {
    let glyph = std::env::args().nth(1).unwrap_or_else(|| "⚙️".into());
    let mut terminal = Terminal::new(CellBackend::new(io::stdout()))?;
    terminal.clear()?;
    // Repeatedly replace a long detail row with a shorter activity row and
    // back, then remove the activity slot. This exercises diff trailing cells.
    for _ in 0..8 {
        for text in [
            "patch the parser".to_owned(),
            format!("{glyph} row"),
            "row".to_owned(),
        ] {
            terminal.draw(|frame| {
                let mut row = frame.area();
                row.y = 3;
                row.height = 1;
                frame.render_widget(Paragraph::new(text), row);
            })?;
            thread::sleep(Duration::from_millis(25));
        }
    }
    // Leave a stable final frame for the PTY harness to capture.
    thread::sleep(Duration::from_secs(3));
    Ok(())
}
