//! UI-themes wave laws (owner spec, ui-themes branch):
//!   §1 launcher-as-session — a compact header band (wordmark · version ·
//!      device) over a top-aligned content column; the BIG centered art and
//!      the shahada stay on the boot splash EXACTLY as before.
//!   §2 palettes — a deliberately designed light mode, a refreshed dark,
//!      `desert` and `oasis`; every surface legible (contrast floors).
//!   §3 theme system — system-default detection (COLORFGBG / OSC 11,
//!      undetectable → dark), the `/theme` numbered arrow-highlight picker,
//!      TUI-local persistence in the profile dir.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppModel, Hit, Screen};
use haider_tui::render::render;
use haider_tui::sanctum::SHAHADA_ARABIC;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

mod common;
use common::launcher_model;

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

// ---- §1: launcher-as-session layout ----

#[test]
fn launcher_renders_header_band_not_centered_art() {
    let model = launcher_model();
    assert_eq!(model.screen, Screen::Launcher);
    let (rows, _, _) = draw(&model, 100, 30);
    // The header band sits AT THE TOP: the compact 16×2 mark art spans
    // band lines 0-1, the product mark + version + device beside it.
    let art = haider_tui::mark::header_rows();
    assert!(
        rows[0].contains(art[0].trim_end()),
        "compact mark, band line 1:\n{}",
        rows.join("\n")
    );
    assert!(rows[1].contains(art[1].trim_end()), "compact mark, line 2");
    assert!(rows[0].contains("haider v"), "wordmark + version in band");
    assert!(
        rows[0].contains(&model.identity.device),
        "device name in band"
    );
    assert!(
        rows[1].contains("provider anthropic"),
        "identity info on band line 2"
    );
    // Band line 3 is the closing frame rule.
    assert!(
        rows[2].chars().filter(|c| *c == '─').count() as u16 >= 90,
        "the band closes with a full-width rule"
    );
    // NO centered art: the 28×4 banner and the shahada are boot ceremony.
    let banner = haider_tui::mark::banner_rows();
    assert!(
        !rows.iter().any(|row| row.contains(banner[2].trim_end())),
        "the big banner may not render on the launcher"
    );
    assert!(
        !rows.iter().any(|row| row.contains(SHAHADA_ARABIC)),
        "the shahada may not render on the launcher"
    );
    // The content column is TOP-ALIGNED under the band, not centered:
    // the recent-sessions head sits directly below the rule's breathing
    // row, in the top quarter of a 30-row frame.
    let recent_y = rows
        .iter()
        .position(|row| row.contains("recent sessions"))
        .expect("recent sessions head");
    assert!(
        recent_y <= 6,
        "content is top-aligned under the band (got row {recent_y})"
    );
}

#[test]
fn boot_splash_keeps_centered_shahada() {
    // §1's second half: ONLY the settled launcher changed — the boot
    // splash keeps the big centered art and the shahada exactly as today.
    let model = AppModel::new();
    assert_eq!(model.screen, Screen::Boot);
    let (rows, _, _) = draw(&model, 100, 30);
    let banner = haider_tui::mark::banner_rows();
    for row in &banner {
        assert!(
            rows.iter().any(|line| line.contains(row.trim_end())),
            "boot keeps the whole big banner (row {row:?})"
        );
    }
    // Centering is measured on the WHOLE 28-cell art block (the map rows
    // carry internal leading blanks that are part of the block).
    let art_ink = banner[2].trim();
    let internal_left = banner[2].chars().take_while(|c| *c == ' ').count();
    let banner_y = rows
        .iter()
        .position(|row| row.contains(art_ink))
        .expect("banner row");
    let ink_col = rows[banner_y].find(art_ink).expect("ink column");
    let ink_col = rows[banner_y][..ink_col].chars().count();
    let block_left = ink_col - internal_left;
    let block_right = 100 - block_left - haider_tui::mark::BANNER_COLS as usize;
    assert!(
        block_left.abs_diff(block_right) <= 2,
        "boot art stays CENTERED (left {block_left}, right {block_right})"
    );
    // The shahada renders on the boot splash once the boot script carries
    // it? No — boot has never drawn the shahada; the LAUNCHER did. The
    // owner's directive keeps the big-art + shahada ceremony in the
    // boot/loading splash: the shahada now renders there, whole or not at
    // all (dignity rule 2).
    assert!(
        rows.iter().any(|row| row.contains(SHAHADA_ARABIC)),
        "the shahada lives on the boot splash:\n{}",
        rows.join("\n")
    );
}

#[test]
fn narrow_boot_omits_the_shahada_whole() {
    // Dignity rule 2 travels with the shahada to its new home: at 24
    // columns no fragment of it may appear — whole or nothing.
    let model = AppModel::new();
    let (rows, _, _) = draw(&model, 24, 20);
    for word in ["الله", "محمد", "رسول"] {
        assert!(
            !rows.iter().any(|row| row.contains(word)),
            "sanctum fragment leaked into a narrow boot frame"
        );
    }
}
