//! tuivirt scroll-model + cache-invariant pins (v0.0.970).
//!
//! The viewport re-architecture (estimated heights, viewport-only layout,
//! bounded cache) may change HOW rows are laid out but not WHICH rows a
//! scroll position shows. These pins state today's scroll semantics as
//! observable frames on a 10k-row replay:
//!
//! * a wheel notch moves the transcript by exactly three rows, a drag
//!   autoscroll step by one, from the bottom, the middle and the top —
//!   clamped at both ends, and an up/down round trip restores the frame
//!   cell-for-cell;
//! * the frame at a scroll position is a pure function of (transcript,
//!   width, theme, `scroll_back`): reaching it through scrolling (warm
//!   cache) and rendering it from a fresh model (cold cache) are identical;
//! * follow mode: at the bottom, new rows appear at the tail; scrolled
//!   back, the view keeps its DISTANCE FROM THE TAIL (`scroll_back` is a
//!   bottom offset) and the chip counts the unseen rows; jump-to-bottom
//!   returns to the exact following frame;
//! * a width change re-wraps from the cache exactly like a fresh render at
//!   the new width, the tail stays anchored, and widening from the top
//!   stays at the top;
//! * edits/appends/completions never leave stale text on screen; a theme
//!   switch re-renders from the cache exactly like a fresh render;
//! * the sticky prompt band's jump puts the producing prompt on the
//!   transcript's top row.
//!
//! Existing neighbours (not duplicated here): `sim_parity_r2_tests::
//! wheel_clamps_to_the_rendered_scroll_range` / `sticky_origin_line_pins_
//! the_prompt_and_click_stays_at_it`, `review2_fix_tests::wheel_before_
//! first_frame_and_resize_never_bank_debt`, `review3_fix_tests::bottom_
//! band_counts_unseen_and_click_returns_to_follow` / `sticky_jump_
//! suppresses_the_bar_until_a_real_wheel`, `qol_drag_autoscroll_tests::*`,
//! `b2b_m3_tree_tests::enter_on_a_node_row_lands_the_render_resolved_jump`
//! / `jump_geometry_survives_wrapping_newlines_wide_glyphs_and_widths` /
//! `jump_resolves_after_resize_with_fresh_geometry`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::RunState;
use haider_tui::app::{AppModel, Hit};
use haider_tui::theme::ThemeKey;

mod tuivirt_common;
use tuivirt_common::{
    Snapshot, agent_row, apply, assert_same_frame, draw, push_agent, push_user, replayed,
    session_model,
};

const BENCH: (u16, u16) = (118, 36);
const SMALL: (u16, u16) = (80, 24);

/// A fresh model at `scroll_back`, with the watermark a following frame
/// would have stamped (so the jump chip reads the same as the warm path).
fn fresh_at(rows: usize, scroll_back: u16) -> AppModel {
    let model = replayed(rows);
    model.bottom_watermark.set(model.projection.entries().len());
    model.scroll_back.set(scroll_back);
    model
}

/// `after` shows the same rows as `before`, moved DOWN the screen by
/// `shift` rows (the reader scrolled up into history).
fn assert_shifted_down(what: &str, before: &Snapshot, after: &Snapshot, shift: usize) {
    let old = before.transcript_interior();
    let new = after.transcript_interior();
    assert_eq!(old.len(), new.len(), "{what}: interior height");
    assert!(old.len() > shift + 4, "{what}: interior too small to compare");
    for i in 0..old.len() - shift {
        assert_eq!(
            new[i + shift],
            old[i],
            "{what}: interior row {} should be the old row {i}",
            i + shift
        );
    }
}

/// One scroll gesture (`wheel` = 3 rows, `drag` = 1 row) exercised from
/// `start`: the forward step moves the interior by `shift` rows or clamps
/// at the top; the reverse step restores the exact frame (or, when the
/// forward step was clamped, moves down by `shift` and the next forward
/// step restores the top frame).
fn exercise_step(
    model: &mut AppModel,
    (width, height): (u16, u16),
    what: &str,
    start: u16,
    max: u16,
    shift: u16,
    step: fn(&mut AppModel, bool),
) {
    model.scroll_back.set(start);
    model.sticky_suppressed.set(false);
    let before = draw(model, width, height);
    assert_eq!(model.scroll_back.get(), start, "{what}: position holds");
    step(model, true);
    let up = draw(model, width, height);
    let expected_up = start.saturating_add(shift).min(max);
    assert_eq!(model.scroll_back.get(), expected_up, "{what}: up offset");
    if expected_up > start {
        assert_shifted_down(&format!("{what} up"), &before, &up, usize::from(shift));
    } else {
        assert_same_frame(&format!("{what} up clamps at the top"), &before, &up);
    }
    step(model, false);
    let down = draw(model, width, height);
    let expected_down = expected_up.saturating_sub(shift);
    assert_eq!(model.scroll_back.get(), expected_down, "{what}: down offset");
    if expected_down == start {
        assert_same_frame(&format!("{what} round trip"), &before, &down);
    } else {
        // Clamped at the top: the down step is a real move, and the
        // top frame is the down frame shifted down by `shift`.
        assert_shifted_down(&format!("{what} down from the top"), &down, &up, usize::from(shift));
        step(model, true);
        let top_again = draw(model, width, height);
        assert_same_frame(&format!("{what} back to the top"), &up, &top_again);
    }
    if start == 0 {
        // The bottom clamps too: a down step at the tail is a no-op frame.
        model.scroll_back.set(0);
        let tail = draw(model, width, height);
        step(model, false);
        let still = draw(model, width, height);
        assert_eq!(model.scroll_back.get(), 0, "{what}: down clamps at the tail");
        assert_same_frame(&format!("{what} down clamps at the tail"), &tail, &still);
    }
}

#[test]
fn wheel_and_drag_steps_from_bottom_middle_and_top_of_10k_rows() {
    for size in [BENCH, SMALL] {
        let (width, height) = size;
        let mut model = replayed(10_000);
        let _ = draw(&model, width, height);
        let max = model.scroll_max.get();
        assert!(max > 1_000, "10k rows overflow {width}x{height} deeply: {max}");
        for (label, start) in [("bottom", 0u16), ("middle", max / 2), ("top", max)] {
            let what = format!("{label} @ {width}x{height}");
            exercise_step(&mut model, size, &format!("wheel {what}"), start, max, 3, AppModel::handle_wheel);
            exercise_step(&mut model, size, &format!("drag {what}"), start, max, 1, AppModel::drag_autoscroll);
        }
        // Anchoring semantics: the bottom shows the last row above the one
        // trailing blank; the top shows the first row on the first content
        // line (at most one leading blank row).
        model.scroll_back.set(0);
        let bottom = draw(&model, width, height);
        let rows = bottom.transcript_rows();
        let last_content = rows
            .iter()
            .rposition(|row| !row.trim().is_empty())
            .expect("content at the tail");
        assert_eq!(
            last_content,
            rows.len() - 2,
            "exactly one blank row between the tail and the band @ {width}x{height}"
        );
        assert!(
            rows[..=last_content]
                .iter()
                .rev()
                .take(3)
                .any(|row| row.contains("row 9999")),
            "the last row is visible at the tail @ {width}x{height}: {rows:?}"
        );
        model.scroll_back.set(max);
        let top = draw(&model, width, height);
        assert_top_of_history(&top, &format!("{width}x{height}"));
    }
}

/// At the very top of history the transcript reads exactly: one blank
/// row, the ` ■ haider` speaker header, then the first entry's text row.
fn assert_top_of_history(frame: &Snapshot, what: &str) {
    let rows = frame.transcript_rows();
    assert!(rows[0].trim().is_empty(), "{what}: line 0 is the block's breathing row: {rows:?}");
    assert!(rows[1].contains("■ haider"), "{what}: line 1 is the speaker header: {rows:?}");
    assert!(rows[2].contains("row 0 —"), "{what}: line 2 is the first entry's text: {rows:?}");
}

#[test]
fn a_scroll_position_renders_identically_warm_and_cold() {
    for (width, height, rows) in [(BENCH.0, BENCH.1, 10_000usize), (SMALL.0, SMALL.1, 3_000)] {
        let warm = replayed(rows);
        let _ = draw(&warm, width, height);
        let max = warm.scroll_max.get();
        for position in [1u16, max / 2, max.saturating_sub(1), max] {
            warm.scroll_back.set(position);
            let via_scroll = draw(&warm, width, height);
            let fresh = fresh_at(rows, position);
            let cold = draw(&fresh, width, height);
            assert_eq!(fresh.scroll_max.get(), max, "same ceiling @ {width}x{height}");
            assert_same_frame(
                &format!("scroll_back={position} @ {width}x{height}"),
                &via_scroll,
                &cold,
            );
        }
    }
}

#[test]
fn follow_mode_and_jump_to_bottom_behave_as_today() {
    let (width, height) = BENCH;
    let mut model = replayed(2_000);
    let following = draw(&model, width, height);
    assert!(!following.has_hit(|hit| matches!(hit, Hit::JumpToBottom)));
    // At the bottom: new rows land at the tail, the offset stays 0.
    for n in 0..5 {
        push_agent(&mut model, &format!("late-{n}"), &format!("late row {n} arrived while following"));
    }
    let tail = draw(&model, width, height);
    assert_eq!(model.scroll_back.get(), 0);
    let rows = tail.transcript_rows();
    assert!(
        rows.iter().rev().take(3).any(|row| row.contains("late row 4")),
        "the newest row is at the tail: {rows:?}"
    );
    assert!(!tail.has_hit(|hit| matches!(hit, Hit::JumpToBottom)));

    // Scrolled back: the view keeps its distance from the tail — appended
    // rows slide the content up by exactly their height — and the chip
    // counts what arrived unseen.
    let scrolled = model.scroll_max.get() / 2;
    model.scroll_back.set(scrolled);
    let before = draw(&model, width, height);
    assert!(before.contains(" Jump to bottom ↓ "));
    assert!(!before.contains("new · Jump to bottom"));
    let max_before = model.scroll_max.get();
    for n in 0..3 {
        push_agent(&mut model, &format!("unseen-{n}"), &format!("unseen row {n} arrived while scrolled"));
    }
    let after = draw(&model, width, height);
    assert_eq!(model.scroll_back.get(), scrolled, "the bottom offset is untouched");
    let appended = usize::from(model.scroll_max.get() - max_before);
    assert!(appended > 0 && appended < 12, "three short rows appended: {appended}");
    assert!(after.contains("3 new · Jump to bottom ↓"), "unseen count");
    let old = before.transcript_interior();
    let new = after.transcript_interior();
    for i in 0..old.len() - appended {
        assert_eq!(new[i], old[i + appended], "interior row {i} slid up by {appended}");
    }
    // Jump to bottom: the exact following frame, chip gone.
    model.handle_hit(Hit::JumpToBottom);
    assert_eq!(model.scroll_back.get(), 0);
    let jumped = draw(&model, width, height);
    assert!(!jumped.has_hit(|hit| matches!(hit, Hit::JumpToBottom)));
    let fresh = fresh_at(2_000, 0);
    let mut fresh = fresh;
    for n in 0..5 {
        push_agent(&mut fresh, &format!("late-{n}"), &format!("late row {n} arrived while following"));
    }
    for n in 0..3 {
        push_agent(&mut fresh, &format!("unseen-{n}"), &format!("unseen row {n} arrived while scrolled"));
    }
    let fresh_frame = draw(&fresh, width, height);
    assert_same_frame("jump-to-bottom == fresh following frame", &jumped, &fresh_frame);
    // Scrolling back again with nothing new: a bare chip.
    model.handle_wheel(true);
    let again = draw(&model, width, height);
    assert!(again.contains(" Jump to bottom ↓ "));
    assert!(!again.contains("new · Jump to bottom"));
}

#[test]
fn resize_rewraps_like_a_fresh_render_and_keeps_the_tail_and_top_anchored() {
    let rows = 3_000usize;
    let mut model = replayed(rows);
    let _ = draw(&model, 118, 36);
    for _ in 0..20 {
        model.handle_wheel(true);
    }
    let mid_118 = draw(&model, 118, 36);
    let offset = model.scroll_back.get();
    assert_eq!(offset, 60);
    // Narrow: the cached re-wrap equals a fresh render at the new width.
    model.handle_resize();
    let narrow = draw(&model, 100, 36);
    assert_eq!(model.scroll_back.get(), offset, "narrowing grows the range; no clamp");
    let fresh = fresh_at(rows, offset);
    assert_same_frame("resized 118→100 == fresh @100", &narrow, &draw(&fresh, 100, 36));
    // Back to 118: the exact earlier frame.
    model.handle_resize();
    let back = draw(&model, 118, 36);
    assert_same_frame("resize round trip", &mid_118, &back);
    // Tail anchoring survives both directions.
    model.scroll_back.set(0);
    for (width, height) in [(80u16, 24u16), (160, 50), (118, 36)] {
        model.handle_resize();
        let frame = draw(&model, width, height);
        let rows = frame.transcript_rows();
        assert!(
            rows.iter().rev().take(4).any(|row| row.contains("row 2999")),
            "tail anchored after resize to {width}x{height}: {rows:?}"
        );
        assert_eq!(model.scroll_back.get(), 0);
    }
    // Widening from the top stays at the top (the range shrinks, the
    // offset clamps to the new ceiling).
    model.scroll_back.set(model.scroll_max.get());
    let top_118 = draw(&model, 118, 36);
    assert_top_of_history(&top_118, "top @ 118x36");
    model.handle_resize();
    let top_160 = draw(&model, 160, 50);
    assert_eq!(model.scroll_back.get(), model.scroll_max.get(), "clamped to the new top");
    assert_top_of_history(&top_160, "widened to 160x50");
    let fresh = fresh_at(rows, u16::MAX);
    assert_same_frame("widened top == fresh top @160", &top_160, &draw(&fresh, 160, 50));
}

#[test]
fn edits_appends_and_completions_never_render_stale_rows() {
    let (width, height) = BENCH;
    let mut model = session_model();
    push_user(&mut model, "stream something");
    apply(&mut model, EventPayload::RunState(RunState::Streaming));
    let item = ItemId::new("stream-1");
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Started {
            item_id: item.clone(),
            item: TurnItem::AgentMessage {
                text: String::new(),
            },
        }),
    );
    for (delta, visible) in [("alpha ", "alpha"), ("beta", "alpha beta")] {
        apply(
            &mut model,
            EventPayload::Item(ItemEvent::Delta {
                item_id: item.clone(),
                delta: ItemDelta::Text {
                    text: delta.to_owned(),
                },
            }),
        );
        let frame = draw(&model, width, height);
        assert!(frame.contains(visible), "the delta shows in the very next frame: {visible:?}");
    }
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: item.clone(),
            item: TurnItem::AgentMessage {
                text: "alpha beta gamma (final)".to_owned(),
            },
        }),
    );
    let frame = draw(&model, width, height);
    assert!(frame.contains("alpha beta gamma (final)"), "completion replaces the streamed text");
    // A tool row flips its glyph the frame after its status flips.
    let call = ItemId::new("tool-1");
    let tool = |status: ToolStatus| TurnItem::ToolCall {
        call_id: "call-1".to_owned(),
        name: "web_fetch".to_owned(),
        args: serde_json::json!({"url": "https://example.invalid/"}),
        status,
    };
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Started {
            item_id: call.clone(),
            item: tool(ToolStatus::InProgress),
        }),
    );
    let running = draw(&model, width, height);
    let row = running.row_containing("web_fetch").expect("the running tool row");
    assert!(!running.rows[row].contains('✓'), "not done yet: {:?}", running.rows[row]);
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: call,
            item: tool(ToolStatus::Completed),
        }),
    );
    let done = draw(&model, width, height);
    let row = done.row_containing("web_fetch").expect("the completed tool row");
    assert!(done.rows[row].contains('✓'), "completed glyph: {:?}", done.rows[row]);
    // A duplicate completion of an EARLIER item id is a no-op frame today
    // (the projection keeps the first commit); pinned so the re-architecture
    // cannot turn a replayed duplicate into a phantom row.
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: item,
            item: TurnItem::AgentMessage {
                text: "replaced entirely".to_owned(),
            },
        }),
    );
    let after_duplicate = draw(&model, width, height);
    assert_same_frame("duplicate completion is a no-op", &done, &after_duplicate);
    // The turn ends: the streaming tail disappears and the frame equals a
    // fresh model fed the same final history.
    apply(&mut model, EventPayload::RunState(RunState::Done));
    let settled = draw(&model, width, height);
    let mut fresh = session_model();
    push_user(&mut fresh, "stream something");
    apply(&mut fresh, EventPayload::RunState(RunState::Streaming));
    push_agent(&mut fresh, "stream-1", "alpha beta gamma (final)");
    apply(
        &mut fresh,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("tool-1"),
            item: tool(ToolStatus::Completed),
        }),
    );
    apply(&mut fresh, EventPayload::RunState(RunState::Done));
    assert_same_frame("edited history == fresh history", &settled, &draw(&fresh, width, height));
}

#[test]
fn theme_switch_rerenders_from_the_cache_like_a_fresh_render() {
    let (width, height) = BENCH;
    let mut model = replayed(500);
    let _ = draw(&model, width, height);
    for _ in 0..5 {
        model.handle_wheel(true);
    }
    let dark = draw(&model, width, height);
    let offset = model.scroll_back.get();
    assert_eq!(offset, 15);
    model.theme = ThemeKey::Light;
    let light = draw(&model, width, height);
    let fresh = fresh_at(500, offset);
    let mut fresh = fresh;
    fresh.theme = ThemeKey::Light;
    assert_same_frame("light via switch == fresh light", &light, &draw(&fresh, width, height));
    model.theme = ThemeKey::Dark;
    let back = draw(&model, width, height);
    assert_same_frame("theme round trip", &dark, &back);
}

#[test]
fn sticky_jump_lands_the_producing_prompt_on_the_transcripts_top_row() {
    for (width, height) in [BENCH, SMALL] {
        let mut model = session_model();
        for turn in 0..30 {
            push_user(&mut model, &format!("prompt {turn} — please continue the work"));
            for n in 0..100 {
                push_agent(&mut model, &format!("j-{turn}-{n}"), &agent_row(turn * 100 + n));
            }
        }
        let _ = draw(&model, width, height);
        let max = model.scroll_max.get();
        model.scroll_back.set(max / 2);
        model.sticky_suppressed.set(false);
        let scrolled = draw(&model, width, height);
        let (rect, hit) = scrolled
            .find_hit(|hit| matches!(hit, Hit::StickyJump(_)))
            .expect("the sticky band offers a jump");
        assert_eq!(rect.y, scrolled.transcript.y, "the band is the transcript's top row");
        let band = scrolled.rows[usize::from(rect.y)].clone();
        let prompt = band
            .find("❯ prompt ")
            .map(|at| band[at..].split(" —").next().unwrap_or_default().to_owned())
            .unwrap_or_else(|| panic!("the band names a prompt: {band:?}"));
        model.handle_hit(hit);
        let landed = draw(&model, width, height);
        let top = landed.transcript_rows()[0].clone();
        assert!(
            top.contains(&prompt),
            "{width}x{height}: the prompt {prompt:?} sits on the top row: {top:?}"
        );
        assert!(
            !landed.has_hit(|hit| matches!(hit, Hit::StickyJump(_))),
            "the band is suppressed after the jump"
        );
        // The landed frame is a pure function of the offset the jump chose.
        let mut fresh = session_model();
        for turn in 0..30 {
            push_user(&mut fresh, &format!("prompt {turn} — please continue the work"));
            for n in 0..100 {
                push_agent(&mut fresh, &format!("j-{turn}-{n}"), &agent_row(turn * 100 + n));
            }
        }
        fresh.bottom_watermark.set(fresh.projection.entries().len());
        fresh.scroll_back.set(model.scroll_back.get());
        fresh.sticky_suppressed.set(true);
        assert_same_frame(
            &format!("sticky jump == fresh @ {width}x{height}"),
            &landed,
            &draw(&fresh, width, height),
        );
    }
}
