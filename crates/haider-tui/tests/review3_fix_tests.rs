//! Review-round-3 fix guards: sacred input at any size (composer cursor
//! row + menu options at 90×10), render as the single scroll authority,
//! sticky suppression after a jump, row-budgeted menu bodies, and true
//! pre-wrap whitespace preservation down to 3-column frames.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::app::{AppEvent, AppModel, Hit, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

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

fn alt_enter() -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row containing {needle:?} not rendered")),
    )
    .expect("row fits u16")
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn menu_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn agent_message(id: &str, text: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(id),
        item: TurnItem::AgentMessage {
            text: text.to_owned(),
        },
    })
}

// ---- P2-1a: the composer's cursor row is sacred at any size ----

#[test]
fn four_line_composer_at_ninety_by_ten_keeps_the_cursor_row() {
    let mut model = session_model();
    for (index, line) in ["aaa", "bbb", "ccc", "ddd"].iter().enumerate() {
        if index > 0 {
            model.handle(alt_enter());
        }
        for c in line.chars() {
            model.handle(key(KeyCode::Char(c)));
        }
    }
    let (rows, _, _) = draw(&model, 90, 10);
    // The allocation grants 3 rows; the composer tail-windows: the cursor
    // row is ALWAYS visible, the hidden head is signalled by ⋮.
    // Directed (TUI5 item 1): the appended ▮ is retired — the cursor row
    // is its TEXT; the caret is a styled cell (asserted in the TUI5 suite).
    assert!(
        rows.iter().any(|row| row.contains("ddd")),
        "cursor row visible at 90×10: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("⋮ bbb")),
        "vertical tail-window marker on the first visible row"
    );
    assert!(
        !rows.iter().any(|row| row.contains("❯ aaa")),
        "the hidden head does not render"
    );
    // Growth still steals from the transcript FIRST when there is room:
    // a taller frame shows all four lines.
    let (rows, _, _) = draw(&model, 90, 24);
    // Directed (TUI5 item 1): "ddd▮" → "ddd" (the caret is a styled cell).
    for needle in ["❯ aaa", "bbb", "ccc", "ddd"] {
        assert!(
            rows.iter().any(|row| row.contains(needle)),
            "{needle} shown"
        );
    }
}

// ---- P2-1b + P2-4: menu options sacred + row-budgeted bodies ----

#[test]
fn menu_options_stay_visible_and_clickable_at_ninety_by_ten() {
    let mut model = menu_model();
    let (rows, hits, _) = draw(&model, 90, 10);
    // Options render — title may stay, hint and body shed first.
    let allow_y = row_of(&rows, "1. Allow once");
    let deny_y = row_of(&rows, "2. Deny");
    assert!(
        !rows.iter().any(|row| row.contains("menu.answer")),
        "hint sheds first under pressure"
    );
    assert!(
        !rows.iter().any(|row| row.contains("fs_edit wants")),
        "body sheds before options"
    );
    // Hit rows come from the RENDERED positions and stay clickable.
    let deny_hit = hits
        .iter()
        .find(|(rect, h)| matches!(h, Hit::MenuOption { index: 1, .. }) && rect.y == deny_y)
        .map(|(_, h)| h.clone())
        .expect("deny option clickable at 90×10");
    let allow_clickable = hits
        .iter()
        .any(|(rect, h)| matches!(h, Hit::MenuOption { index: 0, .. }) && rect.y == allow_y);
    assert!(allow_clickable, "allow option clickable at 90×10");
    model.handle_hit(deny_hit);
    let answer = model.outbox.pop().expect("answer produced");
    assert_eq!(answer.option_key.as_deref(), Some("deny"));
}

#[test]
fn menu_body_wraps_in_narrow_frames_and_options_follow() {
    // At 60 columns the demo card's long path must WRAP (not clip) and the
    // option hit rows must follow the wrapped body down.
    let model = menu_model();
    let (rows, hits, _) = draw(&model, 60, 26);
    let body_start = row_of(&rows, "fs_edit wants to modify");
    // The wrapped remainder — the full path — lands on its own row BELOW
    // the first body row (the transcript's tool row also carries the path,
    // so search after the body start).
    let body_tail = rows
        .iter()
        .enumerate()
        .skip(body_start as usize + 1)
        .find(|(_, row)| row.contains("crates/haider-store/src/event_store.rs"))
        .map(|(index, _)| u16::try_from(index).expect("row fits u16"))
        .expect("wrapped path row below the body start");
    assert!(body_tail > body_start, "body wrapped across rows");
    let allow_y = row_of(&rows, "1. Allow once");
    assert!(allow_y > body_tail, "options sit below the wrapped body");
    assert!(
        hits.iter()
            .any(|(rect, h)| matches!(h, Hit::MenuOption { index: 0, .. }) && rect.y == allow_y),
        "hit row derived from the rendered position"
    );
    // The hint still fits at this height (its tail clips at 60 columns).
    assert!(rows.iter().any(|row| row.contains("↑↓ select · ⏎ confirm")));
}

// ---- P2-3: sticky suppression after a jump ----

#[test]
fn sticky_jump_suppresses_the_bar_until_a_real_wheel() {
    let mut model = session_model();
    // A second prompt (B) with a long reply beneath it.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "second prompt about the follow-up work".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    model.handle(AppEvent::Envelope(Box::new(agent_message(
        "b-reply",
        "line\nline\nline\nline\nline\nline\nline\nline\nline\nline\nline\nline",
    ))));
    let (_, _, _) = draw(&model, 90, 14);
    // One notch up: prompt B is the last prompt above the viewport top.
    model.handle_wheel(true);
    let (rows, hits, _) = draw(&model, 90, 14);
    let sticky_row = &rows[3];
    assert!(
        sticky_row.contains("second prompt about the follow-up work"),
        "sticky pins prompt B: {sticky_row:?}"
    );
    let jump = hits
        .iter()
        .find_map(|(_, h)| match h {
            Hit::StickyJump(_) => Some(h.clone()),
            _ => None,
        })
        .expect("sticky hit");
    model.handle_hit(jump);
    // After the jump: B's REAL row sits at the top and NOTHING pins over
    // it (sim: suppressed until the next real scroll — with prompts A then
    // B, A must not take over the bar).
    let (rows, hits, _) = draw(&model, 90, 14);
    assert!(
        rows[3].contains("❯ second prompt about the follow-up work"),
        "the revealed row is the real prompt row: {:?}",
        rows[3]
    );
    assert!(
        !hits.iter().any(|(_, h)| matches!(h, Hit::StickyJump(_))),
        "no sticky overlay while suppressed"
    );
    // A real wheel notch brings the sticky machinery back.
    model.handle_wheel(true);
    let (_, hits, _) = draw(&model, 90, 14);
    assert!(
        hits.iter().any(|(_, h)| matches!(h, Hit::StickyJump(_))),
        "real scroll lifts the suppression"
    );
}

// ---- 954: the composer queue panel ----

/// The live queue panel (954 owner spec): daemon-held rows render above
/// the composer oldest-top/latest-bottom with a mode label, a mode
/// toggle, and a steer button; clicking steer issues the fenced
/// promotion with the HELD revision; clicking toggle stages the row's
/// verbatim text + next mode (leg two resubmits after the fenced remove
/// commits, so no crash window can silently lose the words); a delta
/// removal drops exactly its row; a conflict clears the staged toggle.
///
/// MUTATION CHECK (executed): make `apply_delta`'s removal arm retain
/// everything (drop the retain) — the `delta removal drops its row`
/// assertion fails; restore, green.
#[test]
fn queue_panel_rows_render_and_act_with_the_held_revision() {
    use haider_protocol::DeliveryMode;
    use haider_protocol::ids::EventId;
    use haider_protocol::queue::{QueueChange, QueueDelta, QueueRow};
    use haider_tui::app::AppRequest;

    let mut model = session_model();
    let row = |suffix: &str, text: &str, mode: DeliveryMode, ordinal: u32| QueueRow {
        id: EventId::new(format!("evt-{suffix}")),
        text: text.into(),
        mode,
        ordinal,
        created_at_ms: 1_000 + u64::from(ordinal),
    };
    model.queue_panel.apply_list(
        vec![
            row("a", "first queued message", DeliveryMode::Queue, 1),
            row("b", "second queued message", DeliveryMode::Subturn, 2),
        ],
        7,
    );
    let (rows, hits, _) = draw(&model, 110, 20);
    let text = rows.join("\n");
    assert!(text.contains("2 held"), "the header counts held rows");
    let first_line = rows
        .iter()
        .position(|line| line.contains("first queued message"))
        .expect("first row renders");
    let second_line = rows
        .iter()
        .position(|line| line.contains("second queued message"))
        .expect("second row renders");
    assert!(
        first_line < second_line,
        "oldest renders above latest (owner spec)"
    );
    assert!(text.contains("(turn end)"), "queue mode labels turn end");
    assert!(
        text.contains("(next tool)"),
        "subturn mode labels next tool"
    );
    let steer_a = hits
        .iter()
        .find_map(|(_, h)| match h {
            Hit::QueueRowSteer(id) if id.as_str() == "evt-a" => Some(h.clone()),
            _ => None,
        })
        .expect("steer hit for row a");
    let toggle_a = hits
        .iter()
        .find_map(|(_, h)| match h {
            Hit::QueueRowToggle(id) if id.as_str() == "evt-a" => Some(h.clone()),
            _ => None,
        })
        .expect("toggle hit for row a");

    // Steer: the fenced promotion carries the HELD revision.
    model.handle_hit(steer_a);
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::QueuePromoteSteer { id, revision: 7 } if id.as_str() == "evt-a"
        )),
        "steer issues the fenced promotion at revision 7"
    );

    // Toggle: leg one removes (fenced), the verbatim text + NEXT mode park
    // in pending_toggle for leg two.
    model.handle_hit(toggle_a);
    let staged = model.queue_panel.pending_toggle.clone().expect("staged");
    assert_eq!(staged.1, "first queued message", "text parks verbatim");
    assert_eq!(
        staged.2,
        DeliveryMode::Subturn,
        "queue toggles to next tool call"
    );
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::QueueToggleRemove { id, revision: 7 } if id.as_str() == "evt-a"
        )),
        "leg one is the fenced remove"
    );

    // A delta removal drops exactly its row and carries the new revision.
    let needs_refresh = model.queue_panel.apply_delta(&QueueDelta {
        revision: 8,
        change: QueueChange::Removed {
            id: EventId::new("evt-b"),
        },
    });
    assert!(!needs_refresh, "a typed removal needs no re-read");
    assert_eq!(model.queue_panel.revision, Some(8));
    assert_eq!(
        model.queue_panel.rows.len(),
        1,
        "delta removal drops its row"
    );
    assert_eq!(model.queue_panel.rows[0].id.as_str(), "evt-a");

    // A conflict names the current revision and clears the staged toggle.
    model.queue_panel.conflicted(11);
    assert_eq!(model.queue_panel.revision, Some(11));
    assert!(
        model.queue_panel.pending_toggle.is_none(),
        "a stale toggle premise is dropped, never replayed blind"
    );
}

// ---- 954: bottom jump band + unseen counter ----

/// The bottom complement of the sticky band (954 owner item): scrolled
/// back, a right-aligned chip offers "Jump to bottom ↓"; entries that land
/// while scrolled back count as "N new"; clicking returns to follow and
/// the next FOLLOWING frame stamps the watermark, clearing the counter.
/// At follow, no band renders.
///
/// MUTATION CHECK (executed): stamp the watermark on EVERY frame (drop the
/// scroll_back == 0 guard in render) — the `2 new` assertion fails at 0;
/// restore, green.
#[test]
fn bottom_band_counts_unseen_and_click_returns_to_follow() {
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(agent_message(
        "long-reply",
        "line\nline\nline\nline\nline\nline\nline\nline\nline\nline\nline\nline",
    ))));
    // Following: watermark stamps, no band.
    let (_, hits, _) = draw(&model, 90, 14);
    assert!(
        !hits.iter().any(|(_, h)| matches!(h, Hit::JumpToBottom)),
        "no band while following"
    );
    // Scroll back, then two entries land unseen.
    model.handle_wheel(true);
    let (_, hits, _) = draw(&model, 90, 14);
    assert!(
        hits.iter().any(|(_, h)| matches!(h, Hit::JumpToBottom)),
        "scrolled back offers the band"
    );
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "unseen prompt".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    model.handle(AppEvent::Envelope(Box::new(agent_message(
        "unseen-reply",
        "unseen answer",
    ))));
    let (rows, hits, _) = draw(&model, 90, 14);
    let band_row = rows.last().map(String::as_str).unwrap_or_default();
    assert!(
        rows.iter()
            .any(|row| row.contains("2 new · Jump to bottom ↓")),
        "two unseen entries count on the band: {band_row:?}"
    );
    // Click: back to follow; the following frame stamps the watermark.
    model.handle_hit(Hit::JumpToBottom);
    assert_eq!(model.scroll_back.get(), 0, "click returns to follow");
    let (_, hits_after, _) = draw(&model, 90, 14);
    assert!(
        !hits_after
            .iter()
            .any(|(_, h)| matches!(h, Hit::JumpToBottom)),
        "no band at follow"
    );
    // Scrolling back again with nothing new: the label carries no count.
    model.handle_wheel(true);
    let (rows, _, _) = draw(&model, 90, 14);
    assert!(
        rows.iter().any(|row| row.contains("Jump to bottom ↓")),
        "band returns on scroll-back"
    );
    assert!(
        !rows.iter().any(|row| row.contains("new · Jump to bottom")),
        "seen entries do not count"
    );
    let _ = hits;
}

// ---- P2-5: true pre-wrap ----

#[test]
fn agent_pre_wrap_preserves_internal_runs_tabs_and_trailing_space() {
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(agent_message(
        "ws-msg",
        "alpha    beta\n\tindented by tab\ntrailing spaces:   ",
    ))));
    let (rows, _, _) = draw(&model, 118, 40);
    assert!(
        rows.iter().any(|row| row.contains("alpha    beta")),
        "internal 4-space run preserved exactly"
    );
    assert!(
        rows.iter().any(|row| row.contains("▏     indented by tab")),
        "tab expands to a fixed 4 cells (documented divergence)"
    );
    // Trailing whitespace survives in the buffer row (the row simply ends
    // in spaces before the frame edge).
    let trailing_y = row_of(&rows, "trailing spaces:");
    assert!(rows[trailing_y as usize].contains("trailing spaces:   "));
}

#[test]
fn rail_survives_five_and_three_column_frames() {
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(agent_message(
        "narrow-msg",
        "abcdef word",
    ))));
    // Width 5: content budget 2 — hard-split rows, each behind the rail.
    let (rows, _, _) = draw(&model, 5, 40);
    let rail_rows: Vec<&String> = rows.iter().filter(|row| row.contains('▏')).collect();
    assert!(rail_rows.len() >= 3, "split rows all carry the rail");
    assert!(rail_rows.iter().any(|row| row.contains("ab")));
    // Width 3: no content column remains — the rail stands alone, and no
    // implicit wrap ever produces a rail-less continuation row.
    let (rows, _, _) = draw(&model, 3, 40);
    assert!(
        rows.iter().any(|row| row.contains('▏')),
        "rail present at 3 columns"
    );
    assert!(
        !rows.iter().any(|row| row.contains("ab")),
        "no content overflow beside a 3-column rail"
    );
}

// ---- regression: launcher composer still sacred with the new ledger ----

#[test]
fn launcher_composer_tail_windows_at_tiny_heights() {
    let mut model = launcher_model();
    for (index, line) in ["one", "two", "three", "four"].iter().enumerate() {
        if index > 0 {
            model.handle(alt_enter());
        }
        for c in line.chars() {
            model.handle(key(KeyCode::Char(c)));
        }
    }
    assert_eq!(model.screen, Screen::Launcher);
    let (rows, _, _) = draw(&model, 90, 8);
    // Directed (TUI5 item 1): "four▮" → "four"; the caret is a styled cell.
    assert!(
        rows.iter().any(|row| row.contains("four")),
        "launcher cursor row sacred at 90×8: {rows:?}"
    );
}
