//! TUI4 — the owner's v0.0.9 screenshot wave: no auto-start on attach, the
//! composer band as one complete panel, the always-present SubTree main row,
//! the half-block حيدر mark, the capped launcher column, the todos panel's
//! spacing/hover/collapse, and the background-agent waiting line.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, Hit, Screen};
use haider_tui::projection::TranscriptEntry;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

mod common;
use common::{launcher_model, submit};

struct Frame {
    rows: Vec<String>,
    hits: Vec<(ratatui::layout::Rect, Hit)>,
    buffer: ratatui::buffer::Buffer,
}

impl Frame {
    fn row_of(&self, needle: &str) -> usize {
        self.rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row containing {needle:?} in\n{}", self.rows.join("\n")))
    }

    fn has(&self, needle: &str) -> bool {
        self.rows.iter().any(|row| row.contains(needle))
    }

    fn bg(&self, x: u16, y: u16) -> Color {
        self.buffer[(x, y)].style().bg.expect("a background")
    }
}

fn draw(model: &AppModel, width: u16, height: u16) -> Frame {
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
    Frame { rows, hits, buffer }
}

// ---- Item 1: opening a session starts nothing ----

#[test]
fn attaching_a_session_replays_its_seed_and_starts_no_turn() {
    // MUTATION CHECK: restore the old `AppRequest::AttachSample` push (or any
    // other turn kick-off) in `attach_sample` and this fails on
    // `turn_active` / `requests` — the assertions below are not satisfiable
    // by an empty transcript either, because the seed rows must be present.
    let mut model = launcher_model();
    common::hit_session_named(&mut model, "billing-service");
    assert_eq!(model.screen, Screen::Session);
    assert!(!model.turn_active, "attach must not start a turn");
    // TUI4c (directed): attach requests NOTHING at all — no teardown
    // either, because the previous session keeps running in its slot
    // (sim `openSession`, tui.js:1606: "attaching never cancels a turn").
    assert_eq!(model.requests, vec![]);
    // The SEEDED transcript is what the session opens with (sim tui.js:474).
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::User { text, .. }
            if text == "wire stripe webhooks into the billing service and backfill the missing invoices"
    )));
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "◇ checkpoint 7 committed"
    )));
    assert_eq!(
        model.session_dir, "~/dev/diffforge/cloud",
        "sim session.dir"
    );
    assert!(model.projection.context_tokens() > 0, "meter seeded too");
    assert_eq!(model.projection.badge(), "IDLE", "no run is live");

    // The l1 seed owns the sim's running web-index chip; attaching it shows
    // that chip without starting a parent turn.
    let mut model = launcher_model();
    common::hit_session_named(&mut model, "l1-remote-projects");
    assert!(!model.turn_active);
    assert_eq!(model.chips.len(), 1, "the seeded chip came with it");
    assert_eq!(model.chips[0].name, "web-index");
    assert_eq!(model.chips[0].callsign, "Salman");
}

#[test]
fn an_untouched_launcher_never_plays_anything() {
    // MUTATION CHECK: re-add the 6 s AutoPlay timer and this fails — the
    // reducer no longer has an event that can start a turn on its own.
    let model = launcher_model();
    assert_eq!(model.screen, Screen::Launcher);
    assert!(!model.turn_active);
    assert!(model.projection.entries().is_empty());
    assert!(model.requests.is_empty());
}

// ---- Item 2: the composer band is ONE complete panel ----

#[test]
fn the_composer_band_fills_its_whole_region_and_closes_with_a_rule() {
    // MUTATION CHECK: drop the `Block::default().style(input_style)` ground
    // in `render_composer` and the per-cell background sweep below fails;
    // drop the closing rule and the frame-rule assertion fails.
    let mut model = launcher_model();
    submit(&mut model, "walk me through the harness");
    let frame = draw(&model, 118, 34);
    let composer_y = frame.row_of("message haider") as u16;
    let band = model.theme.theme().input_bg;
    let expected = Color::Rgb(band.r, band.g, band.b);
    // EVERY cell of the composer row carries the band, edge to edge.
    // Directed (TUI5 item 1): cell x=4 (pad 2 + sigil 2) is the CURSOR
    // cell — a gold ground by the new cursor law, the one deliberate
    // exception to the band sweep.
    let gold = model.theme.theme().gold;
    for x in 0..118 {
        if x == 4 {
            assert_eq!(
                frame.bg(x, composer_y),
                Color::Rgb(gold.r, gold.g, gold.b),
                "cursor cell wears the gold cursor ground"
            );
            continue;
        }
        assert_eq!(
            frame.bg(x, composer_y),
            expected,
            "composer row cell {x} is outside the band"
        );
    }
    // S2 item 4 FLIP (was TUI4 item 2's padding-row sweep): the pad row
    // is retired — the band rests at exactly ONE text row and the closing
    // rule sits DIRECTLY beneath the composer (owner screenshot: the band
    // read two rows tall at rest).
    let closing = frame.rows[composer_y as usize + 1].clone();
    assert!(
        closing.chars().filter(|c| *c == '─').count() >= 100,
        "no closing rule directly under the band, got {closing:?}"
    );
    assert_ne!(
        frame.bg(0, composer_y + 1),
        expected,
        "the closing rule sits OUTSIDE the band"
    );
    // The gold top rule is still the band's opening edge.
    assert!(frame.rows[composer_y as usize - 1].contains('─'));
}

// ---- Item 3: the SubTree main row ----

#[tokio::test(start_paused = true)]
async fn the_subtree_main_row_is_always_present_and_marks_the_current_node() {
    let mut model = launcher_model();
    submit(&mut model, "use a subagent for the webhook tests");
    model.requests.clear();
    // Seed a chip directly: this test is about the panel, not the driver.
    model.chips.push(haider_tui::app::ChipModel::from_seed(
        haider_tui::mock::sample_seed_chip(2).expect("seed chip"),
    ));
    let frame = draw(&model, 118, 34);
    // Present on the SESSION screen too (owner item 3; the sim shows it only
    // in the subagent view).
    assert!(
        frame.has("⌂"),
        "the main row joins the panel on the session"
    );
    assert!(
        frame.hits.iter().any(|(_, hit)| *hit == Hit::SessionHome),
        "and it is clickable"
    );
    let home_y = frame.row_of("back to the main transcript") as u16;
    let bold = frame.buffer[(4, home_y)]
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD);
    assert!(bold, "on the main transcript, the main row is bold");

    // Viewing a chip moves the bold to that chip.
    model.handle_hit(Hit::ChipRow("seed-l1-sub".to_owned()));
    assert_eq!(model.screen, Screen::Subagent);
    let frame = draw(&model, 118, 34);
    let home_y = frame.row_of("back to the main transcript") as u16;
    assert!(
        !frame.buffer[(4, home_y)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "off the main transcript, the main row is not bold"
    );
    let chip_y = frame.row_of("Salman (r) · web-index") as u16;
    let chip_x = frame.rows[chip_y as usize]
        .find("Salman")
        .expect("chip name") as u16;
    assert!(
        frame.buffer[(chip_x, chip_y)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the viewed chip is bold"
    );
}

#[test]
fn the_subtree_main_row_takes_hover_chrome_like_a_chip_row() {
    let mut model = launcher_model();
    submit(&mut model, "use a subagent here");
    model.requests.clear();
    model.chips.push(haider_tui::app::ChipModel::from_seed(
        haider_tui::mock::sample_seed_chip(2).expect("seed chip"),
    ));
    let plain = draw(&model, 118, 34);
    let home_y = plain.row_of("back to the main transcript") as u16;
    let before = plain.bg(60, home_y);
    model.handle_hover(Some(Hit::SessionHome));
    let hovered = draw(&model, 118, 34);
    let after = hovered.bg(60, home_y);
    let sel = model.theme.theme().sel_bg;
    assert_eq!(after, Color::Rgb(sel.r, sel.g, sel.b), "selBg hover band");
    assert_ne!(before, after, "hover changes the row");
}

// ---- Item 4: the half-block حيدر mark ----

#[test]
fn the_mark_maps_and_their_half_block_rendering_agree() {
    // MUTATION CHECK: flip any pixel in `mark::BANNER` and this fails — the
    // map is the authoritative bitmap and the rendering is derived from it,
    // so the two can never drift apart silently.
    for (map, rows) in [
        (
            haider_tui::mark::BANNER.as_slice(),
            haider_tui::mark::banner_rows(),
        ),
        (
            haider_tui::mark::HEADER.as_slice(),
            haider_tui::mark::header_rows(),
        ),
    ] {
        assert_eq!(rows.len(), map.len() / 2, "two pixel rows per terminal row");
        let width = map.iter().map(|row| row.len()).max().unwrap_or(0);
        for (index, row) in rows.iter().enumerate() {
            let top: Vec<char> = format!("{:<width$}", map[index * 2]).chars().collect();
            let bottom: Vec<char> = format!("{:<width$}", map[index * 2 + 1]).chars().collect();
            for (x, cell) in row.chars().enumerate() {
                let expected = match (top[x] == '#', bottom[x] == '#') {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                };
                assert_eq!(cell, expected, "map/render mismatch at row {index} col {x}");
            }
        }
    }
    assert_eq!(
        haider_tui::mark::banner_rows().len() as u16,
        haider_tui::mark::BANNER_ROWS
    );
    assert_eq!(
        haider_tui::mark::header_rows().len() as u16,
        haider_tui::mark::HEADER_ROWS
    );
    // Ink only — a map may never leak a stray character into the frame.
    for row in haider_tui::mark::banner_rows() {
        assert!(row.chars().all(|c| "█▀▄ ".contains(c)), "half blocks only");
    }
}

#[test]
fn the_banner_renders_whole_or_falls_back_to_the_text_mark() {
    // MUTATION CHECK: widen `BANNER_COLS`/`BANNER_MARGIN` without moving the
    // gate, or clip instead of falling back, and one of these two halves
    // fails — the mark is never partially drawn.
    //
    // UI-themes wave flip: the big banner is BOOT-SPLASH ceremony now (the
    // settled launcher wears the compact header band), so the tier law is
    // pinned on the boot screen, where the banner still lives.
    let model = AppModel::new();
    assert_eq!(model.screen, Screen::Boot);
    let wide = draw(&model, 118, 34);
    let banner = haider_tui::mark::banner_rows();
    for row in &banner {
        assert!(
            wide.rows.iter().any(|line| line.contains(row.trim_end())),
            "banner row {row:?} missing at a wide frame"
        );
    }
    assert!(!wide.has("حيدر"), "the text mark yields to the art");
    // Exactly at the threshold the art still renders whole.
    let threshold = haider_tui::mark::BANNER_COLS + haider_tui::mark::BANNER_MARGIN * 2;
    assert!(haider_tui::mark::banner_fits(threshold));
    assert!(!haider_tui::mark::banner_fits(threshold - 1));
    // One cell under it, the mark steps down a tier — never a clipped map.
    let narrow = draw(&model, threshold - 1, 34);
    assert!(narrow.has("حيدر"), "the text mark returns");
    for row in &banner {
        let ink = row.trim_end();
        assert!(
            !narrow.rows.iter().any(|line| line.contains(ink)),
            "a banner row leaked into a frame too narrow for it"
        );
    }
}

#[test]
fn the_session_header_mark_spans_both_lines_or_yields() {
    let mut model = launcher_model();
    submit(&mut model, "walk me through the harness");
    let frame = draw(&model, 118, 34);
    let rows = haider_tui::mark::header_rows();
    assert!(
        frame.rows[0].contains(rows[0].trim_end()),
        "line 1 of the mark"
    );
    assert!(
        frame.rows[1].contains(rows[1].trim_end()),
        "line 2 of the mark"
    );
    assert!(frame.rows[0].contains("[ ← main ]"), "beside the back chip");
    assert!(frame.rows[0].contains("haider v"), "and the info block");
    // Too tight → the one-line text mark, never a clipped map.
    let tight = draw(&model, 50, 34);
    assert!(tight.has("حيدر"));
    assert!(!tight.rows[0].contains(rows[0].trim_end()));
}

// ---- Item 5: the launcher column cap ----

#[test]
fn the_launcher_column_is_capped_and_centered_at_a_wide_frame() {
    // HONEST FLIP (ui-launcher-fixes, owner screenshot): the ui-themes
    // wave left-anchored this block against the header band's edge; the
    // owner reversed it — the capped column now CENTERS horizontally in a
    // wide frame (the header/composer bands stay full-width; ≤-cap widths
    // keep the old geometry — `launcher_fixes_tests` pins those). The cap
    // survives — a 165-col frame must not stretch rows across it.
    let model = launcher_model();
    let frame = draw(&model, 165, 40);
    let cap = 70usize;
    let mut edges = Vec::new();
    for needle in [
        "recent sessions",
        "billing-service",
        "l1-remote-projects",
        "◉ Aura",
        "⚿ Accounts",
        "⇄ Peers",
    ] {
        let row = &frame.rows[frame.row_of(needle)];
        // TUI4d item 14: every block row leads with a one-cell rail column
        // (the sim's `.rail` sliver, tui.js:4370-4394) — blank on idle
        // rows, the ▎ shimmer glyph on a running one. It is PADDING, not
        // text: the shared TEXT edge starts after it.
        let start = row.chars().take_while(|c| *c == ' ' || *c == '▎').count();
        let ink = row.trim_end().chars().count() - start;
        assert!(
            ink <= cap,
            "{needle:?} spans {ink} cells, over the {cap}-cell column"
        );
        edges.push(start);
    }
    // The `.recent` block is ONE column: every row shares a left edge…
    assert!(
        edges.windows(2).all(|pair| pair[0] == pair[1]),
        "the recent block is not one column: {edges:?}"
    );
    // …and that column CENTERS in the frame: the pad derives from the cap
    // ((165-70)/2 = 47), the rail cell sits on the pad's edge, so the
    // shared text edge is 48 — never the old left anchor's 1.
    assert_eq!(edges[0], 48, "the column centers: pad 47 + the rail cell");
    // The identity info and dir moved INTO the header band (line 2).
    let info_row = frame.row_of("provider anthropic");
    assert_eq!(info_row, 1, "identity info lives on header line 2");
    assert!(
        frame.rows[info_row].contains("dir ~/dev"),
        "dir in the band"
    );
}

// ---- Item 7: the todos panel ----

#[tokio::test(start_paused = true)]
async fn the_todos_panel_hovers_collapses_and_keeps_its_spacer() {
    let mut model = launcher_model();
    submit(&mut model, "plan todo the harness work");
    model.requests.clear();
    let (generic, roster) = (
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(3),
    );
    for beat in &haider_tui::script::respond_beats(
        "plan todo the harness work",
        false,
        haider_protocol::DeliveryMode::Steer,
        1,
        &generic,
        &roster,
    ) {
        if let haider_tui::script::Beat::Emit(payload) = beat {
            model.handle(AppEvent::Envelope(Box::new(payload.clone())));
            if model.projection.todos().is_some_and(|t| t.pinned) {
                break;
            }
        }
    }
    let frame = draw(&model, 118, 34);
    let header_y = frame.row_of("▾ todos");
    // (a) a blank breathing row separates the panel from the stream above.
    assert!(
        frame.rows[header_y - 1].trim().is_empty(),
        "no spacer above the todos panel"
    );
    // (b) the header is a hit target and takes hover chrome, as do the rows.
    assert!(frame.hits.iter().any(|(_, hit)| *hit == Hit::TodosToggle));
    assert!(
        frame
            .hits
            .iter()
            .any(|(_, hit)| matches!(hit, Hit::TodoRow(_)))
    );
    model.handle_hover(Some(Hit::TodosToggle));
    let hovered = draw(&model, 118, 34);
    let sel = model.theme.theme().sel_bg;
    assert_eq!(
        hovered.bg(60, header_y as u16),
        Color::Rgb(sel.r, sel.g, sel.b),
        "header hover band"
    );
    model.handle_hover(Some(Hit::TodoRow(1)));
    let hovered = draw(&model, 118, 34);
    assert_eq!(
        hovered.bg(60, header_y as u16 + 2),
        Color::Rgb(sel.r, sel.g, sel.b),
        "item row hover band"
    );
    model.handle_hover(None);
    // (c) clicking the header collapses it to the sim's one-line summary.
    model.handle_hit(Hit::TodosToggle);
    assert!(model.todos_collapsed);
    let collapsed = draw(&model, 118, 34);
    let header = &collapsed.rows[collapsed.row_of("todos —")];
    assert!(header.contains("▸ todos"), "arrow flips, got {header:?}");
    assert!(header.contains(" · ■ "), "collapsed shows the current item");
    assert!(
        !collapsed.has("☐ run the suite and report"),
        "item rows are gone"
    );
    model.handle_hit(Hit::TodosToggle);
    assert!(!model.todos_collapsed);
    assert!(draw(&model, 118, 34).has("☐ run the suite and report"));

    // (d) at 90×10 the breathing rows shed and the composer still shows.
    let tight = draw(&model, 90, 10);
    // Directed (TUI5 item 1): the empty composer's appended ▮ became a styled
    // CELL over a space — "❯  " (sigil + cursor cell) is the signature now.
    assert!(tight.has("❯  "), "the cursor row is sacred");
}

// ---- Item 8: the background-agent waiting line ----

#[test]
fn the_waiting_line_tracks_live_background_agents() {
    let mut model = launcher_model();
    submit(&mut model, "use a subagent here");
    model.requests.clear();
    assert!(
        !draw(&model, 118, 34).has("background agent"),
        "no line without live agents"
    );
    model.chips.push(haider_tui::app::ChipModel::from_seed(
        haider_tui::mock::sample_seed_chip(2).expect("seed chip"),
    ));
    let one = draw(&model, 118, 34);
    assert!(
        one.has("✳ Waiting for 1 background agent to finish"),
        "singular line"
    );
    // A second live agent pluralizes.
    let mut second = haider_tui::mock::sample_seed_chip(2).expect("seed chip");
    second.agent = "seed-two".to_owned();
    model
        .chips
        .push(haider_tui::app::ChipModel::from_seed(second));
    assert!(
        draw(&model, 118, 34).has("✳ Waiting for 2 background agents to finish"),
        "plural line"
    );
    // A chip waiting on the USER says so instead.
    model.chips[1].state = haider_tui::script::ChipDisplayState::InputRequired;
    assert!(
        draw(&model, 118, 34).has("✳ Waiting for 2 background agents — 1 needs input"),
        "needs-input variant"
    );
    // Finished agents stop being waited on.
    model.chips[1].state = haider_tui::script::ChipDisplayState::Done;
    model.chips[0].state = haider_tui::script::ChipDisplayState::Done;
    assert!(!draw(&model, 118, 34).has("background agent"));
}

#[test]
fn the_breathing_rows_shed_before_any_sacred_row() {
    let mut model = launcher_model();
    submit(&mut model, "use a subagent here");
    model.requests.clear();
    model.chips.push(haider_tui::app::ChipModel::from_seed(
        haider_tui::mock::sample_seed_chip(2).expect("seed chip"),
    ));
    for (width, height) in [(90, 10), (90, 7), (90, 5), (90, 1)] {
        let frame = draw(&model, width, height);
        assert!(
            // Directed (TUI5 item 1): the empty composer's appended ▮ became a styled
            // CELL over a space — "❯  " (sigil + cursor cell) is the signature now.
            frame.has("❯  "),
            "the composer's cursor row survives {width}×{height}"
        );
    }
    // At the tight sizes the waiting line and the panels are gone, but the
    // frame is still coherent — nothing is half-drawn.
    let tiny = draw(&model, 90, 5);
    assert!(!tiny.has("background agent"));
}
