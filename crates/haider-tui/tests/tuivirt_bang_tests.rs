//! User shell commands own the same breathing row as other user actions.
//! Existing collapse goldens stay unchanged: these pins exercise provenance,
//! which the older model-command fixtures deliberately do not carry.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{
    CommandExecutionOrigin, ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem,
    UserCommandOriginV1,
};
use haider_tui::app::{AppEvent, AppModel, Hit};
use haider_tui::toolfold::{RowState, ToolTiming};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod tuivirt_common;
use tuivirt_common::{apply, check_golden, draw, push_agent, session_model};

fn submit(model: &mut AppModel, text: &str) {
    for c in text.chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
}

fn command(model: &mut AppModel, id: &str, user: bool, completed: bool) {
    let item = |status, exit_code| TurnItem::CommandExecution {
        call_id: format!("call-{id}"),
        command: "pwd".into(),
        status,
        exit_code,
    };
    apply(
        model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new(id),
            item: item(ToolStatus::InProgress, None),
        }),
    );
    if user {
        // Same metadata application as the live/replay envelope handler.
        assert!(model.projection.mark_user_command(&UserCommandOriginV1 {
            origin: CommandExecutionOrigin::UserCommand,
            command_item_id: ItemId::new(id),
            call_id: format!("call-{id}"),
        }));
    }
    apply(
        model,
        EventPayload::Item(ItemEvent::Delta {
            item_id: ItemId::new(id),
            delta: ItemDelta::CommandOutput {
                stream: OutputStream::Stdout,
                chunk_b64: base64::engine::general_purpose::STANDARD
                    .encode(b"/workspace\nsecond line\n"),
            },
        }),
    );
    if completed {
        apply(
            model,
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(id),
                item: item(ToolStatus::Completed, Some(0)),
            }),
        );
    }
    model.tool_timings.insert(
        id.into(),
        ToolTiming {
            started_ms: model.clock_ms - 35,
            ended_ms: completed.then_some(model.clock_ms),
        },
    );
}

fn bang_model(completed: bool) -> AppModel {
    let mut model = session_model();
    model.clock_ms = 1_700_000_000_000;
    push_agent(&mut model, "reply", "The workspace is ready.");
    command(&mut model, "bang", true, completed);
    model
}

/// MUTATION: remove the user-command blank row in tool_disclosure_lines.
/// These frames and the preceding-row assertion fail, including narrow mode.
#[test]
fn bang_spacing_goldens_and_disclosure_hits() {
    let model = bang_model(true);
    for (width, height) in [(32, 24), (80, 24), (120, 40)] {
        let frame = draw(&model, width, height);
        let row = frame
            .rows
            .iter()
            .position(|row| row.contains("! pwd"))
            .expect("bang row");
        assert!(frame.rows[row - 1].trim().is_empty());
        assert!(frame.rows[row - 2].contains("ready."));
        assert!(frame.hits.iter().any(|(rect, hit)| {
            rect.y as usize == row && matches!(hit, Hit::ToolRowToggle(id) if id == "bang")
        }));
        assert!(
            !frame.hits.iter().any(|(rect, hit)| {
                rect.y as usize == row - 1 && matches!(hit, Hit::ToolRowToggle(_))
            }),
            "spacing is not a disclosure target"
        );
        check_golden("bang_separated", &frame);
    }
}

#[test]
fn bang_spacing_survives_live_rows_verbosity_collapse_and_resize() {
    for completed in [false, true] {
        for mode in [
            "/verbosity quiet",
            "/verbosity normal",
            "/verbosity verbose",
            "/collapse",
            "/collapse off",
        ] {
            let mut model = bang_model(completed);
            submit(&mut model, mode);
            for width in [120, 32, 80, 120] {
                let frame = draw(&model, width, 40);
                let row = frame
                    .rows
                    .iter()
                    .position(|row| row.contains("! pwd"))
                    .expect(mode);
                assert!(
                    frame.rows[row - 1].trim().is_empty(),
                    "{mode} width={width}"
                );
                assert!(frame.hits.iter().any(|(rect, hit)| {
                    rect.y as usize == row && matches!(hit, Hit::ToolRowToggle(id) if id == "bang")
                }));
            }
        }
    }
}

#[test]
fn user_command_fold_has_one_gap_and_never_absorbs_model_calls() {
    let mut model = session_model();
    model.clock_ms = 1_700_000_000_000;
    command(&mut model, "model", false, true);
    command(&mut model, "first", true, true);
    command(&mut model, "second", true, true);
    let runs = haider_tui::render::foldable_runs(&model.projection, &model.toolfold);
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].start, runs[0].len), (1, 2));
    let frame = draw(&model, 80, 40);
    let row = frame
        .rows
        .iter()
        .position(|row| row.contains("Ran 2 shell commands"))
        .expect("fold");
    assert!(frame.rows[row - 1].trim().is_empty());
    assert!(frame.rows[row - 2].contains("workspace"));
    let hit = frame
        .hits
        .iter()
        .find(|(rect, hit)| {
            rect.y as usize == row && matches!(hit, Hit::ToolFoldToggle(id) if id == "first")
        })
        .expect("fold hit after spacing")
        .1
        .clone();
    model.handle_hit(hit);
    let unfolded = draw(&model, 80, 40);
    assert_eq!(
        unfolded
            .rows
            .iter()
            .filter(|row| row.contains("! pwd"))
            .count(),
        2
    );
    for (index, line) in unfolded.rows.iter().enumerate() {
        if line.contains("! pwd") {
            assert!(unfolded.rows[index - 1].trim().is_empty());
        }
    }
    model.toolfold.set("first", RowState::ShowAll);
    let expanded = draw(&model, 32, 40);
    assert!(expanded.rows.iter().any(|row| row.contains("second line")));
}

#[test]
fn shell_inventory_notification_does_not_flash_an_unsupported_frame_error() {
    let frame = haider_rpc::WireFrame::ShellInventoryChanged {
        inventory: haider_rpc::ShellInventoryWire {
            count: 1,
            revision: 1,
        },
    };
    assert!(haider_tui::link::map_frame(frame).is_empty());
}
