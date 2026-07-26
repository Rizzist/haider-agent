//! Round-2 sim-parity guards: esc-interrupt, sticky origin line, wheel
//! clamp, ghost completion, /theme arg slots, paste tokenization, mid-turn
//! echo, auto-title notes, transcript typography (maroon sigil · gold agent
//! header · rail · struck todos), badge tones, boot alignment, help body,
//! ⌃G, and selection wrap-around.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::theme::ThemeKey;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

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

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
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

// ---- G40: esc-interrupt ----

#[test]
fn esc_mid_turn_interrupts_and_stays_on_the_session() {
    let mut model = launcher_model();
    model.handle(key(KeyCode::Char('1')));
    assert!(model.turn_active);
    model.requests.clear();
    // The script's user message flips to the session view.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "fix the failing boundary test in haider-store".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    assert_eq!(model.screen, Screen::Session);

    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session, "interrupt stays put");
    assert!(!model.turn_active);
    assert_eq!(model.requests, vec![AppRequest::Interrupt]);
    assert!(model.projection.interrupted());
    let (rows, _, _) = draw(&model, 118, 34);
    assert!(rows.iter().any(|row| row.contains("⏸ IDLE (i)")), "badge");
    assert!(
        rows.iter()
            .any(|row| row.contains("· interrupted — run → cancelled · idle (i)")),
        "transcript note"
    );

    // Typing decays idle(i) → idle (sim runStates decay).
    model.handle(key(KeyCode::Char('x')));
    assert!(!model.projection.interrupted());

    // Idle esc walks back to the launcher.
    model.composer.clear();
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Launcher);
}

// ---- G16 + G26: wheel clamp + sticky origin line ----

#[test]
fn wheel_clamps_to_the_rendered_scroll_range() {
    let mut model = session_model();
    let (_, _, _) = draw(&model, 90, 14);
    let max = model.scroll_max.get();
    assert!(max > 0, "demo transcript overflows a 14-row frame");
    for _ in 0..50 {
        model.handle_wheel(true);
    }
    assert_eq!(model.scroll_back, max, "wheel-up stops at the top");
    model.handle_wheel(false);
    assert_eq!(model.scroll_back, max.saturating_sub(3), "no wound debt");
}

#[test]
fn sticky_origin_line_pins_the_producing_prompt_and_click_returns() {
    let mut model = session_model();
    let (_, _, _) = draw(&model, 90, 14);
    model.handle_wheel(true);
    let (rows, hits, terminal) = draw(&model, 90, 14);
    // The sticky line sits on the transcript's top row (header 2 + rule 1).
    let sticky_y = 3u16;
    assert!(
        rows[sticky_y as usize].contains("❯ fix the failing boundary test in haider-store"),
        "sticky pins the producing prompt: {:?}",
        rows[sticky_y as usize]
    );
    let theme = model.theme.theme();
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(0, sticky_y)].bg,
        Color::from(theme.bar_bg),
        "sticky band ground"
    );
    let (rect, _) = hits
        .iter()
        .find(|(_, h)| *h == Hit::StickyJump)
        .expect("sticky hit region");
    assert_eq!(rect.y, sticky_y);
    model.handle_hit(Hit::StickyJump);
    assert_eq!(model.scroll_back, 0, "click returns to the live tail");
    // Back at the bottom, no sticky (and no hit) renders.
    let (_, hits, _) = draw(&model, 90, 14);
    assert!(!hits.iter().any(|(_, h)| *h == Hit::StickyJump));
}

#[test]
fn wheel_is_inert_under_the_help_overlay() {
    let mut model = session_model();
    let (_, _, _) = draw(&model, 90, 14);
    model.help_open = true;
    model.handle_wheel(true);
    assert_eq!(model.scroll_back, 0, "hidden transcript never scrolls");
}

// ---- G11 + G12: ghost completion + /theme arg slot ----

#[test]
fn ghost_completes_the_highlighted_row_inline() {
    let mut model = session_model();
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(model.ghost().as_deref(), Some("heme"));
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let composer_y = row_of(&rows, "❯ /t▮heme");
    assert!(rows[composer_y as usize].contains("⇥ tab"), "tab tag");
    let buffer = terminal.backend().buffer();
    let ghost_x = col_of(&rows[composer_y as usize], "heme");
    assert_eq!(buffer[(ghost_x, composer_y)].fg, Color::from(theme.dim));
    let tab_x = col_of(&rows[composer_y as usize], "⇥");
    assert_eq!(buffer[(tab_x, composer_y)].fg, Color::from(theme.faint));
    // The ghost follows the highlighted row (Down → /tree).
    model.handle(key(KeyCode::Down));
    assert_eq!(model.ghost().as_deref(), Some("ree"));
}

#[test]
fn theme_arg_slot_offers_completes_and_runs() {
    let mut model = launcher_model();
    for c in "/theme ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let labels: Vec<String> = model.palette_items().iter().map(|i| i.label()).collect();
    assert_eq!(labels, ["dawn", "ivory", "dark"]);
    let (rows, hits, _) = draw(&model, 118, 34);
    assert!(rows.iter().any(|row| row.contains("Ivory Light")), "descs");
    // Ghost offers the first slot value after the trailing space.
    assert_eq!(model.ghost().as_deref(), Some("dawn"));
    // ⏎ on a highlighted arg row completes and RUNS.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.theme, ThemeKey::Ivory, "/theme ivory executed");
    // Click path: the third arg row runs /theme dark.
    for c in "/theme ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(hits.iter().any(|(_, h)| *h == Hit::PaletteRow(2)));
    model.handle_hit(Hit::PaletteRow(2));
    assert_eq!(model.theme, ThemeKey::Dark, "clicked arg row executed");
    // Fragment filtering: /theme i → ivory only; tab completes in place.
    for c in "/theme i".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let labels: Vec<String> = model.palette_items().iter().map(|i| i.label()).collect();
    assert_eq!(labels, ["ivory"]);
    model.handle(key(KeyCode::Tab));
    assert_eq!(model.composer, "/theme ivory");
}

// ---- G15: paste tokenization ----

#[test]
fn big_pastes_become_pill_tokens_and_render_gold() {
    let mut model = launcher_model();
    model.handle(AppEvent::Paste("a\nb\nc\nd\ne".to_owned()));
    assert_eq!(model.composer, "[Pasted 5 lines] ");
    for c in "ship it".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    // The echoed user row (from the canned turn's UserMessage) styles the
    // token gold on the gold-soft ground.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "[Pasted 5 lines] ship it".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    // The TRANSCRIPT row (the header title echoes the text too, in dim).
    let user_y = row_of(&rows, "❯ [Pasted 5 lines] ship it");
    let token_x = col_of(&rows[user_y as usize], "[Pasted");
    let cell = &terminal.backend().buffer()[(token_x, user_y)];
    assert_eq!(cell.fg, Color::from(theme.gold));
    assert_eq!(cell.bg, Color::from(theme.gold_soft));

    // Small pastes stay literal text.
    let mut model = launcher_model();
    model.handle(AppEvent::Paste("a\nb".to_owned()));
    assert_eq!(model.composer, "a b");
}

// ---- G51 + G47: mid-turn echo + auto-title note ----

#[test]
fn mid_turn_submit_flashes_and_echoes_a_note() {
    let mut model = launcher_model();
    model.handle(key(KeyCode::Char('1')));
    assert!(model.turn_active);
    // The script's user message flips to the session view mid-turn.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "fix the failing boundary test in haider-store".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    for c in "also update the docs".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(
        model
            .flash
            .as_deref()
            .unwrap_or("")
            .contains("already running")
    );
    let (rows, _, _) = draw(&model, 140, 34);
    assert!(
        rows.iter()
            .any(|row| row.contains("⧗ mid-turn input — “also update the docs”")),
        "typed text is not lost"
    );
}

#[test]
fn auto_title_uses_auto_blurb_and_notes_the_transcript() {
    let mut model = launcher_model();
    for c in "please fix the flaky boundary test suite before the release".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    // Sim autoBlurb: first seven words, capitalized.
    assert_eq!(
        model.session_title.as_deref(),
        Some("Please fix the flaky boundary test suite")
    );
    // The note lands right after the echoed user row.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "please fix the flaky boundary test suite before the release".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    let (rows, _, _) = draw(&model, 140, 34);
    let user_y = row_of(&rows, "❯ please fix the flaky");
    let note_y = row_of(
        &rows,
        "· session titled — “Please fix the flaky boundary test suite”",
    );
    assert_eq!(note_y, user_y + 1, "note directly under the prompt");
}

// ---- G27/G28/G29/G31/G32: transcript typography ----

#[test]
fn transcript_typography_matches_the_sim() {
    let model = session_model();
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    // User sigil: MAROON bold (gold belongs to the composer).
    let user_y = row_of(&rows, "❯ fix the failing boundary test");
    let sig_x = col_of(&rows[user_y as usize], "❯");
    let sig = &buffer[(sig_x, user_y)];
    assert_eq!(sig.fg, Color::from(theme.maroon));
    assert!(sig.modifier.contains(Modifier::BOLD));
    // Agent header: entirely GOLD; body behind a gold-soft rail.
    let who_y = row_of(&rows, "■ haider");
    let who_x = col_of(&rows[who_y as usize], "■");
    assert_eq!(buffer[(who_x, who_y)].fg, Color::from(theme.gold));
    assert_eq!(buffer[(who_x + 2, who_y)].fg, Color::from(theme.gold));
    let body_y = row_of(&rows, "▏ Reading the failing test first");
    let rail_x = col_of(&rows[body_y as usize], "▏");
    assert_eq!(buffer[(rail_x, body_y)].fg, Color::from(theme.gold_soft));
    // Tool row: maroon name + dim desc from the args.
    let tool_y = row_of(&rows, "✓ fs_read crates/haider-store/src/event_store.rs");
    let name_x = col_of(&rows[tool_y as usize], "fs_read");
    assert_eq!(buffer[(name_x, tool_y)].fg, Color::from(theme.maroon));
    let desc_x = col_of(&rows[tool_y as usize], "crates/haider-store");
    assert_eq!(buffer[(desc_x, tool_y)].fg, Color::from(theme.dim));
    // Completed plan: ok header card + struck faint rows.
    let plan_y = row_of(&rows, "☑ plan completed — 3 todos");
    let plan_x = col_of(&rows[plan_y as usize], "☑");
    assert_eq!(buffer[(plan_x, plan_y)].fg, Color::from(theme.ok));
    let item_y = row_of(&rows, "✓ read the failing test");
    let text_x = col_of(&rows[item_y as usize], "read the failing test");
    let cell = &buffer[(text_x, item_y)];
    assert_eq!(cell.fg, Color::from(theme.faint));
    assert!(cell.modifier.contains(Modifier::CROSSED_OUT), "struck");
}

#[test]
fn compaction_row_is_gold_and_honest() {
    use haider_protocol::ids::ArtifactRef;
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Completed {
            item_id: ItemId::new("compact-1"),
            item: TurnItem::ContextCompaction {
                summary_artifact: ArtifactRef::new("blake3:abc"),
            },
        },
    ))));
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let y = row_of(&rows, "⊟ context compacted — summary retained");
    let x = col_of(&rows[y as usize], "⊟");
    assert_eq!(
        terminal.backend().buffer()[(x, y)].fg,
        Color::from(theme.gold)
    );
}

// ---- G34: pinned-todo state styling + dep tags ----

#[test]
fn pinned_todos_style_by_state_and_show_dep_tags() {
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Started {
            item_id: ItemId::new("plan-dep"),
            item: TurnItem::Plan {
                items: vec![
                    TodoItem {
                        id: 0,
                        text: "land the fix".to_owned(),
                        state: TodoState::Processing,
                        dep: None,
                    },
                    TodoItem {
                        id: 1,
                        text: "re-run the suite".to_owned(),
                        state: TodoState::Listed,
                        dep: Some(0),
                    },
                    TodoItem {
                        id: 2,
                        text: "close the ticket".to_owned(),
                        state: TodoState::Completed,
                        dep: None,
                    },
                ],
            },
        },
    ))));
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    // Header: dim label, gold count.
    let hdr_y = row_of(&rows, "▾ todos — 1/3 done");
    let count_x = col_of(&rows[hdr_y as usize], "1/3");
    assert_eq!(buffer[(count_x, hdr_y)].fg, Color::from(theme.gold));
    // Processing: gold mark + bright text.
    let cur_y = row_of(&rows, "■ land the fix");
    let cur_x = col_of(&rows[cur_y as usize], "land the fix");
    assert_eq!(buffer[(cur_x, cur_y)].fg, Color::from(theme.bright));
    // Dep-blocked: faint text + `· after #1` tag.
    let dep_y = row_of(&rows, "re-run the suite · after #1");
    let dep_x = col_of(&rows[dep_y as usize], "re-run");
    assert_eq!(buffer[(dep_x, dep_y)].fg, Color::from(theme.faint));
    // Completed: struck faint text behind an ok mark.
    let done_y = row_of(&rows, "✓ close the ticket");
    let done_x = col_of(&rows[done_y as usize], "close the ticket");
    let cell = &buffer[(done_x, done_y)];
    assert_eq!(cell.fg, Color::from(theme.faint));
    assert!(cell.modifier.contains(Modifier::CROSSED_OUT));
}

// ---- G37: badge tones on the wire ----

#[test]
fn badge_renders_idle_dim_and_permission_warn_outline() {
    let model = launcher_model();
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let status_y = u16::try_from(rows.len() - 1).expect("status row");
    let idle_x = col_of(&rows[status_y as usize], "[ IDLE ]");
    assert_eq!(
        terminal.backend().buffer()[(idle_x + 2, status_y)].fg,
        Color::from(theme.dim),
        "plain IDLE is quiet dim"
    );

    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let (rows, _, terminal) = draw(&model, 118, 34);
    let status_y = u16::try_from(rows.len() - 1).expect("status row");
    let badge_x = col_of(&rows[status_y as usize], "? PERMISSION_REQUIRED");
    let cell = &terminal.backend().buffer()[(badge_x, status_y)];
    assert_eq!(cell.fg, Color::from(theme.warn), "warn OUTLINE ink");
    assert_eq!(cell.bg, Color::from(theme.bar_bg), "no warn fill");
}

// ---- G42: boot alignment ----

#[test]
fn boot_checks_align_as_a_column_and_subline_is_gold() {
    let mut model = AppModel::new();
    for payload in demo_script().into_iter().take(3) {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    // All check markers start at the same column (left-aligned block).
    let done_y = row_of(&rows, "✓ store open");
    let pending_y = row_of(&rows, "· worker warm");
    assert_eq!(
        col_of(&rows[done_y as usize], "✓ store open"),
        col_of(&rows[pending_y as usize], "· worker warm"),
        "glyph column aligns"
    );
    let sub_y = row_of(&rows, "· starting up");
    let sub_x = col_of(&rows[sub_y as usize], "v0.");
    assert_eq!(
        terminal.backend().buffer()[(sub_x, sub_y)].fg,
        Color::from(theme.gold),
        "boot subline is gold"
    );
}

// ---- G46 + G41 + G36 + G45: help body, ⌃G, wrap, /theme listing ----

#[test]
fn help_carries_the_sim_menus_and_keys_explainers() {
    let mut model = launcher_model();
    for c in "/help".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    let (rows, _, _) = draw(&model, 140, 40);
    assert!(
        rows.iter()
            .any(|row| row.contains("menus — every card (permission · hook trust · update"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("keys — ⏎ send · ⇧⏎ newline · esc interrupt / back"))
    );
}

#[test]
fn ctrl_g_flashes_the_tokens_stub() {
    let mut model = session_model();
    model.handle(ctrl('g'));
    assert!(
        model
            .flash
            .as_deref()
            .unwrap_or("")
            .contains("/tokens — UI ready"),
        "⌃G reserves the token-panel binding honestly"
    );
}

#[test]
fn palette_and_menu_selection_wrap_around() {
    // Palette: /t in session → 4 rows; Up from 0 wraps to the last.
    let mut model = session_model();
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Up));
    assert_eq!(model.palette_selection, 3);
    model.handle(key(KeyCode::Down));
    assert_eq!(model.palette_selection, 0, "down from last wraps home");

    // Menu options wrap too.
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    assert!(model.projection.open_menu().is_some());
    model.handle(key(KeyCode::Up));
    assert_eq!(model.menu_selection, 1, "up from 0 wraps to the last");
    model.handle(key(KeyCode::Down));
    assert_eq!(model.menu_selection, 0);
}

#[test]
fn bare_theme_cycles_and_lists_the_choices() {
    let mut model = launcher_model();
    assert_eq!(model.theme, ThemeKey::Dawn);
    for c in "/theme".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.theme, ThemeKey::Ivory, "bare /theme still cycles");
    let flash = model.flash.as_deref().unwrap_or("");
    assert!(flash.contains("theme → Ivory Light"));
    assert!(flash.contains("themes — dawn · ivory · dark"));
}
