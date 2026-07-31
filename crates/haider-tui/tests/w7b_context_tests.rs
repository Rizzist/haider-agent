//! W7b context-truth laws: footprint extension items feed the meter and
//! /tokens (never the transcript), the compaction intent becomes the
//! pre-announce note, and /tree lists the main line.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::history::COMPACTION_INTENT_EXTENSION_KIND;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::app::{AppModel, Hit, Screen};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;

use haider_tui::app::AppEvent;
use haider_tui::mock::demo_script;

mod common;
use common::key;

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Vec<(Rect, Hit)>) {
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
    (rows, hits)
}

fn footprint(truth: ContextFootprintTruth) -> ContextFootprint {
    ContextFootprint {
        input_tokens: 90_000,
        output_tokens: 8_000,
        cached_input_tokens: 2_000,
        used_tokens: 100_000,
        context_window: Some(200_000),
        reserved_output_tokens: 30_000,
        soft_threshold_tokens: Some(170_000),
        estimated_turns_to_threshold: Some(7),
        truth,
    }
}

fn apply_extension(model: &mut AppModel, id: &str, item: TurnItem) {
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new(id),
            item: item.clone(),
        }));
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item,
        }));
}

/// MUTATION CHECK: remove the `consume_context_extension` guard from
/// `SessionProjection::apply_item`. Expected runtime failure: the
/// transcript grows a `⋯ context_footprint_v1` row and the row-count
/// assertion below fails.
#[test]
fn footprint_snapshots_feed_the_meter_and_never_the_transcript() {
    let mut model = session_model();
    let rows_before = model.projection.entries().len();
    let item = footprint(ContextFootprintTruth::Estimated)
        .extension_item()
        .expect("carrier");
    apply_extension(&mut model, "fp-1", item);
    assert_eq!(
        model.projection.entries().len(),
        rows_before,
        "a footprint snapshot is never a transcript row"
    );
    let held = model.projection.latest_footprint().expect("footprint held");
    assert_eq!(held.used_tokens, 100_000);
    // The status-bar meter carries the snapshot with the ~ honesty mark.
    let (rows, _) = draw(&model, 118, 34);
    let meter_row = rows
        .iter()
        .find(|row| row.contains("tok ·"))
        .expect("meter row");
    assert!(
        meter_row.contains("~100k tok") && meter_row.contains("of 200k"),
        "estimated snapshot meters with ~ against ITS window: {meter_row}"
    );
}

/// MUTATION CHECK: make the meter ignore `ContextFootprintTruth` (always
/// estimated or always exact). Expected runtime failure: one of the two
/// assertions below.
#[test]
fn the_meter_drops_the_tilde_only_for_exact_truth() {
    let mut model = session_model();
    let item = footprint(ContextFootprintTruth::Exact)
        .extension_item()
        .expect("carrier");
    apply_extension(&mut model, "fp-exact", item);
    let (rows, _) = draw(&model, 118, 34);
    let meter_row = rows
        .iter()
        .find(|row| row.contains("tok ·"))
        .expect("meter row");
    assert!(
        meter_row.contains("100k tok") && !meter_row.contains("~100k tok"),
        "exact truth meters plainly: {meter_row}"
    );
}

/// MUTATION CHECK: drop the intent arm from `consume_context_extension`.
/// Expected runtime failure: the pre-announce note vanishes (and a
/// `⋯ context_compaction_intent_v1` row appears instead).
#[test]
fn the_compaction_intent_becomes_the_preannounce_note() {
    let mut model = session_model();
    let footprint_item = footprint(ContextFootprintTruth::Exact)
        .extension_item()
        .expect("carrier");
    apply_extension(&mut model, "fp-2", footprint_item);
    apply_extension(
        &mut model,
        "intent-1",
        TurnItem::Extension {
            kind: COMPACTION_INTENT_EXTENSION_KIND.to_owned(),
            data: serde_json::json!({"resume": "auto_mid_turn"}),
        },
    );
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter()
            .any(|row| row.contains("· context at 50% — compacting")),
        "the intent marker pre-announces with the live percent"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains(COMPACTION_INTENT_EXTENSION_KIND)),
        "no raw ⋯ extension row leaks"
    );
}

/// MUTATION CHECK: break the /tokens truth split (always `~`). Expected
/// runtime failure: the exact-panel assertion below.
#[test]
fn the_token_panel_prints_footprint_splits_with_truth_honesty() {
    let mut model = session_model();
    let item = footprint(ContextFootprintTruth::Exact)
        .extension_item()
        .expect("carrier");
    apply_extension(&mut model, "fp-3", item);
    model.token_panel = true;
    let (rows, _) = draw(&model, 130, 40);
    let panel_row = rows
        .iter()
        .find(|row| row.contains("in ") && row.contains("cached "))
        .expect("panel row");
    assert!(
        panel_row.contains("in 90k")
            && panel_row.contains("out 8.0k")
            && panel_row.contains("cached 2.0k")
            && panel_row.contains("≈7 turns to auto-compaction")
            && !panel_row.contains('~'),
        "exact splits print plainly with the turns estimate: {panel_row}"
    );
    assert!(
        rows.iter().any(|row| row.contains("context by model")),
        "panel header"
    );
}

/// MUTATION CHECK: drop the user or compaction arm from
/// `haider_tui::app::tree_rows`. Expected runtime failure: the missing
/// row assertion below.
#[test]
fn tree_lists_the_main_line_turns_and_compactions() {
    let mut model = session_model();
    apply_extension(
        &mut model,
        "compaction-1",
        TurnItem::ContextCompaction {
            summary_artifact: haider_protocol::ids::ArtifactRef::new("blake3:tree"),
            tokens_before: Some(160_000),
            tokens_after: Some(30_000),
        },
    );
    for c in "/tree".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Tree);
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter().any(|row| row.contains("SESSION TREE —")),
        "tree header"
    );
    assert!(
        rows.iter().any(|row| row.contains("├─ ❯")),
        "user turns list as nodes"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("⊟ compacted 160k → 30k")),
        "compaction nodes carry their counts"
    );
    // esc returns to the session.
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session);
}
