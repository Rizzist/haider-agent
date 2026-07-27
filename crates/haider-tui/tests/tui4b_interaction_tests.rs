//! TUI4b — the owner's interaction pack: in-app drag-select + auto-copy
//! (item 9), ⌃C as navigation (item 10), and the sticky origin band's
//! chrome + hover (item 11).
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
use haider_tui::select::{Selection, selection_text};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn launcher_model() -> AppModel {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

struct Frame {
    rows: Vec<String>,
    hits: Vec<(Rect, Hit)>,
    buffer: Buffer,
}

impl Frame {
    fn row_of(&self, needle: &str) -> usize {
        self.rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row containing {needle:?} in\n{}", self.rows.join("\n")))
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

// ---- Item 9: extraction ----

#[test]
fn selection_text_flows_linearly_and_trims_row_padding() {
    // MUTATION CHECK: make the range rectangular (first..last columns on
    // every row) and the middle row loses its left half; skip the
    // pad-trim and every row gains trailing spaces.
    let buffer = Buffer::with_lines(["alpha beta      ", "  middle row    ", "tail row here   "]);
    let selection = Selection {
        anchor: (6, 0),
        head: (3, 2),
        dragging: false,
    };
    assert_eq!(
        selection_text(&buffer, &selection),
        "beta\n  middle row\ntail",
        "first row from the anchor, whole middle row, last row to the head — \
         trailing pad spaces trimmed per row"
    );
}

#[test]
fn selection_text_normalizes_an_upward_drag_and_bounds_to_the_frame() {
    let buffer = Buffer::with_lines(["one   ", "two   "]);
    // Dragged UP-and-left: head before anchor — same range either way.
    let up = Selection {
        anchor: (2, 1),
        head: (0, 0),
        dragging: false,
    };
    let down = Selection {
        anchor: (0, 0),
        head: (2, 1),
        dragging: false,
    };
    assert_eq!(selection_text(&buffer, &up), selection_text(&buffer, &down));
    assert_eq!(selection_text(&buffer, &up), "one\ntwo");
    // A head reported past the frame (resize race) clamps, never panics.
    let wild = Selection {
        anchor: (0, 0),
        head: (500, 500),
        dragging: false,
    };
    assert_eq!(selection_text(&buffer, &wild), "one\ntwo");
}

#[test]
fn selection_text_skips_wide_glyph_continuation_cells() {
    // MUTATION CHECK: read every cell instead of advancing by symbol width
    // and the wide glyphs gain phantom pad characters between them.
    let buffer = Buffer::with_lines(["汉字 ok   "]);
    let selection = Selection {
        anchor: (0, 0),
        head: (7, 0),
        dragging: false,
    };
    assert_eq!(selection_text(&buffer, &selection), "汉字 ok");
}

// ---- Item 9: click vs drag through the production dispatch ----

#[test]
fn a_plain_click_still_dispatches_its_hit_on_release() {
    // MUTATION CHECK: dispatch the hit on Down again and the drag test
    // below fails (the suppressed click acts anyway); drop the Up dispatch
    // and THIS fails.
    let mut model = session_model();
    let frame = draw(&model, 118, 34);
    let (rect, _) = frame
        .hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::BackChip))
        .expect("back chip hit");
    let (x, y) = (rect.x + 1, rect.y);
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    assert_eq!(
        model.screen,
        Screen::Session,
        "Down alone is only a potential anchor — the click resolves on Up"
    );
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Up(MouseButton::Left), x, y),
    );
    assert_eq!(model.screen, Screen::Launcher, "the released click acted");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::CopySelection)),
        "a click copies nothing"
    );
}

#[test]
fn a_drag_selects_copies_and_suppresses_the_click_hit() {
    // MUTATION CHECK: fire the pending click on Up even when a selection
    // exists and `todos_collapsed` flips; skip the CopySelection request
    // and the auto-copy contract is gone.
    let mut model = session_model();
    let frame = draw(&model, 118, 34);
    let (rect, _) = frame
        .hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::BackChip))
        .expect("back chip hit");
    let (x, y) = (rect.x + 1, rect.y);
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), x + 12, y),
    );
    let live = model.selection.expect("drag entered selection mode");
    assert!(live.dragging);
    assert_eq!(live.anchor, (x, y));
    assert_eq!(live.head, (x + 12, y));
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Up(MouseButton::Left), x + 12, y),
    );
    assert_eq!(
        model.screen,
        Screen::Session,
        "a drag that selected suppresses the click-hit on release"
    );
    let done = model.selection.expect("the highlight survives the copy");
    assert!(!done.dragging);
    assert_eq!(
        model
            .requests
            .iter()
            .filter(|request| matches!(request, AppRequest::CopySelection))
            .count(),
        1,
        "release requested exactly one auto-copy"
    );
}

#[test]
fn same_cell_jitter_is_a_click_not_a_drag() {
    // Terminals emit Drag events for sub-cell motion too; movement that
    // never leaves the anchor cell must stay a click.
    let mut model = session_model();
    let frame = draw(&model, 118, 34);
    let (rect, _) = frame
        .hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::BackChip))
        .expect("back chip hit");
    let (x, y) = (rect.x + 1, rect.y);
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), x, y),
    );
    assert!(
        model.selection.is_none(),
        "same-cell jitter selects nothing"
    );
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Up(MouseButton::Left), x, y),
    );
    assert_eq!(model.screen, Screen::Launcher, "the click still lands");
}

#[test]
fn the_highlight_clears_on_the_next_click_or_keypress() {
    let mut model = session_model();
    let frame = draw(&model, 118, 34);
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Down(MouseButton::Left), 4, 5),
    );
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), 20, 6),
    );
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Up(MouseButton::Left), 20, 6),
    );
    assert!(model.selection.is_some());
    // A keypress clears the finished highlight…
    model.handle(key(KeyCode::Char('x')));
    assert!(model.selection.is_none(), "keypress clears the selection");
    // …and so does the next press, before it resolves to click or drag.
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Down(MouseButton::Left), 4, 5),
    );
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), 9, 5),
    );
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Up(MouseButton::Left), 9, 5),
    );
    assert!(model.selection.is_some());
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Down(MouseButton::Left), 40, 8),
    );
    assert!(model.selection.is_none(), "the next press clears it");
}

#[test]
fn the_selection_highlight_paints_the_linear_range_only() {
    // MUTATION CHECK: paint a rectangle instead and the cells outside the
    // linear flow (left of the anchor on its row, right of the head on
    // its row) light up too.
    let mut model = session_model();
    model.selection = Some(Selection {
        anchor: (10, 5),
        head: (6, 7),
        dragging: false,
    });
    let frame = draw(&model, 118, 34);
    let theme = model.theme.theme();
    let sel: Color = theme.sel_bg.into();
    // First row: from the anchor column to the right edge.
    assert_eq!(frame.buffer[(10, 5)].bg, sel);
    assert_eq!(frame.buffer[(117, 5)].bg, sel);
    assert_ne!(frame.buffer[(9, 5)].bg, sel, "left of the anchor: unlit");
    // Middle row: the whole width.
    assert_eq!(frame.buffer[(0, 6)].bg, sel);
    assert_eq!(frame.buffer[(117, 6)].bg, sel);
    // Last row: from the left edge to the head column.
    assert_eq!(frame.buffer[(0, 7)].bg, sel);
    assert_eq!(frame.buffer[(6, 7)].bg, sel);
    assert_ne!(frame.buffer[(7, 7)].bg, sel, "right of the head: unlit");
    // Outside the range: untouched.
    assert_ne!(frame.buffer[(10, 4)].bg, sel);
    assert_ne!(frame.buffer[(10, 8)].bg, sel);
}

// ---- Item 10: ⌃C is navigation ----

#[test]
fn ctrl_c_walks_a_session_back_to_the_launcher_and_touches_no_turn() {
    // MUTATION CHECK: restore `should_quit = true` for every screen and
    // this fails on `screen`; make ⌃C interrupt and it fails on
    // `turn_active` / the absence of an Interrupt request.
    let mut model = launcher_model();
    for c in "walk me through the harness".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "walk me through the harness".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    assert_eq!(model.screen, Screen::Session);
    assert!(model.turn_active);
    model.requests.clear();
    model.listening = false;

    model.handle(ctrl('c'));
    assert_eq!(model.screen, Screen::Launcher, "⌃C navigates");
    assert!(!model.should_quit, "…and does NOT quit from a session");
    assert!(
        model.turn_active,
        "navigation only — the running turn keeps its lifecycle"
    );
    assert!(
        model.requests.is_empty(),
        "no Interrupt request — esc owns interrupt"
    );
    assert!(
        !model.projection.entries().is_empty(),
        "the session transcript survives: still resumable"
    );
}

#[test]
fn ctrl_c_quits_from_the_launcher_and_from_boot() {
    let mut model = launcher_model();
    assert_eq!(model.screen, Screen::Launcher);
    model.handle(ctrl('c'));
    assert!(model.should_quit, "launcher ⌃C quits, as before");

    let mut boot = AppModel::new();
    assert_eq!(boot.screen, Screen::Boot);
    boot.handle(ctrl('c'));
    assert!(boot.should_quit, "boot has no launcher to return to");
}

#[test]
fn ctrl_c_resets_the_view_state_on_its_way_out() {
    // Subagent view: the path clears so a later session entry starts at
    // the main transcript, not a stale child view.
    let mut model = session_model();
    model.screen = Screen::Subagent;
    model.view_path = vec!["t1-docs".to_owned()];
    model.listening = true;
    model.handle(ctrl('c'));
    assert_eq!(model.screen, Screen::Launcher);
    assert!(model.view_path.is_empty(), "view state reset");
    assert!(!model.listening, "talk hold cancelled (P1-3)");

    // The help overlay counts as a covered surface too.
    let mut covered = session_model();
    covered.help_open = true;
    covered.handle(ctrl('c'));
    assert_eq!(covered.screen, Screen::Launcher);
    assert!(!covered.help_open, "overlay closed");
    assert!(!covered.should_quit);

    // Aura: back to the launcher; the aura surface itself persists.
    let mut aura = launcher_model();
    aura.screen = Screen::Aura;
    aura.handle(ctrl('c'));
    assert_eq!(aura.screen, Screen::Launcher);
    assert!(!aura.should_quit);
}

// ---- Item 11: the sticky origin band ----

fn scrolled_session() -> (AppModel, Frame) {
    let mut model = session_model();
    let _ = draw(&model, 90, 14);
    model.handle_wheel(true);
    let frame = draw(&model, 90, 14);
    (model, frame)
}

#[test]
fn the_sticky_band_is_visually_distinct_with_a_bottom_frame_edge() {
    // Chrome per the sim's ACTUAL CSS (StickyLine, tui.js:4597-4623 —
    // owner item 11): `background: ${bg}f0` + `border-bottom: 1px solid
    // frame`. DIRECTED PARITY CHANGE: an earlier review round read this as
    // bare theme ground with no underline and the cell test pinned that;
    // the owner's read of the rendered sim wins — the band takes the barBg
    // tint (a terminal cell cannot alpha-blend a covered row) and the
    // border-bottom ports as the frame-colored underline.
    // MUTATION CHECK: ground the row in `theme.bg` again, or drop the
    // UNDERLINED modifier, and this fails.
    let (model, frame) = scrolled_session();
    let theme = model.theme.theme();
    let y = u16::try_from(frame.row_of("❯ fix the failing boundary test in haider-store"))
        .expect("row fits u16");
    for x in [0u16, 44, 89] {
        let cell = &frame.buffer[(x, y)];
        assert_eq!(
            cell.bg,
            Color::from(theme.bar_bg),
            "the band ground spans the full row (col {x})"
        );
        assert!(
            cell.style().add_modifier.contains(Modifier::UNDERLINED),
            "the bottom frame edge spans the full row (col {x})"
        );
    }
    let row = &frame.rows[y as usize];
    let sig_x = u16::try_from(
        row.char_indices()
            .position(|(_, c)| c == '❯')
            .expect("sigil"),
    )
    .expect("col fits u16");
    let sig = &frame.buffer[(sig_x, y)];
    assert_eq!(sig.fg, Color::from(theme.maroon));
    assert!(sig.modifier.contains(Modifier::BOLD));
    let text_x = sig_x + 2;
    assert_eq!(
        frame.buffer[(text_x, y)].fg,
        Color::from(theme.bright),
        "prompt text stays bright when not hovered"
    );
}

#[test]
fn the_sticky_band_hovers_through_the_standard_path_and_reverts() {
    // Sim `&:hover` (tui.js:4614-4617): opaque ground + maroon ink — a
    // real visual hover, not just cursor:pointer. MUTATION CHECK: skip the
    // hover branch in the sticky render and the hovered frame is
    // indistinguishable from the resting one.
    let (mut model, frame) = scrolled_session();
    let theme = model.theme.theme();
    let (rect, _) = frame
        .hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::StickyJump(_)))
        .expect("sticky hit region");
    dispatch_input(
        &mut model,
        &frame.hits,
        mouse(MouseEventKind::Moved, rect.x + 3, rect.y),
    );
    assert!(matches!(model.hovered, Some(Hit::StickyJump(_))));
    let hovered = draw(&model, 90, 14);
    let y = rect.y;
    let cell = &hovered.buffer[(0, y)];
    assert_eq!(
        cell.bg,
        Color::from(theme.sel_bg),
        "hover shifts the band to the selection ground"
    );
    assert!(
        cell.style().add_modifier.contains(Modifier::UNDERLINED),
        "the bottom edge survives the hover"
    );
    let row = &hovered.rows[y as usize];
    let sig_x = u16::try_from(
        row.char_indices()
            .position(|(_, c)| c == '❯')
            .expect("sigil"),
    )
    .expect("col fits u16");
    assert_eq!(
        hovered.buffer[(sig_x + 2, y)].fg,
        Color::from(theme.maroon),
        "hover turns the prompt text maroon (sim &:hover color)"
    );
    // Pointer leaves: the band reverts through the same path.
    dispatch_input(&mut model, &frame.hits, mouse(MouseEventKind::Moved, 0, 12));
    let rested = draw(&model, 90, 14);
    assert_eq!(rested.buffer[(0, y)].bg, Color::from(theme.bar_bg));
}

#[test]
fn rendered_selection_text_reads_a_real_frame_not_the_swapped_buffer() {
    // Regression, caught live in a PTY: the copy path once read the
    // terminal's `current_buffer_mut()` AFTER a draw — ratatui swaps and
    // resets that buffer, so every copy extracted "" (empty pbcopy, empty
    // OSC 52 payload). The path now re-renders the model into a scratch
    // buffer; this pins non-empty, exact text end to end.
    let model = session_model();
    let frame = draw(&model, 118, 34);
    let y = u16::try_from(frame.row_of("❯ fix the failing boundary test in haider-store"))
        .expect("row fits u16");
    let selection = Selection {
        anchor: (1, y),
        head: (47, y),
        dragging: false,
    };
    let text = haider_tui::runtime::rendered_selection_text(
        &model,
        ratatui::layout::Size::new(118, 34),
        &selection,
    );
    assert_eq!(text, "❯ fix the failing boundary test in haider-store");
}
