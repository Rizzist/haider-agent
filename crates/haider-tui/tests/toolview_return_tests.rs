//! 973-tui-toolview repair 5 — "Esc back identical", at EVERY scroll offset.
//!
//! Verify 5 found the full-detail view returning the reader ~30 rows away
//! from where they opened it, whenever the transcript was scrolled back:
//! the door click focused the row, the focus bumped the disclosure revision,
//! the sparse layout cache re-seeded its height ESTIMATES, and the
//! bottom-relative `scroll_back` then pointed at different content. These
//! pins drive a transcript shaped like the verifier's live session (eleven
//! turns, a 1,300-line write, hidden request-budget items, more than the
//! cache's eager-measure threshold of entries) through the real `AppModel`
//! and renderer, and require, for every wheel position from the bottom to
//! the top:
//!
//! - every drawn `⎿` line is a click target that opens that row's detail
//!   (door hit-testing at every offset);
//! - opening by click, or by keyboard focus + ⌃O, and closing with Esc,
//!   ⌃O, q, ⏎ or the `esc back` header returns a frame that is
//!   cell-for-cell identical (text AND style) to the one before.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::{ItemId, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_protocol::request_budget::{
    PROVIDER_REQUEST_BUDGET_EXTENSION_KIND, RequestBudgetContinuationV1, RequestBudgetPhaseV1,
    RequestBudgetStatusV1, RequestBudgetV1,
};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_tui::app::{AppEvent, AppModel, Hit};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod tuivirt_common;
use base64::Engine as _;
use tuivirt_common::{Snapshot, apply, draw, push_agent, push_user, session_model};

const WORKSPACE: &str = "/private/tmp/haider-r5/ws";

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn result(preview: &str, status: ToolResultStatus, reason: Option<&str>) -> BoundedResult {
    BoundedResult {
        preview: preview.to_owned(),
        truncated: false,
        truncation: None,
        effects: Vec::new(),
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status,
        reason: reason.map(str::to_owned),
        presentation: None,
        orchestration: None,
    }
}

struct Builder {
    model: AppModel,
    budget: usize,
}

impl Builder {
    fn new() -> Self {
        let mut model = session_model();
        model.cwd = WORKSPACE.to_owned();
        model.clock_ms = 1_700_000_000_000;
        model.launch_origin = Some((
            1,
            Some(format!(
                "{WORKSPACE} · Workspace: /private/tmp/haider-r5/profile/workspaces/Haider/1448-04-12/s-81f76312274245224b6b8765ba86ed29"
            )),
        ));
        Self { model, budget: 0 }
    }

    /// A hidden request-budget tally: an entry that renders no rows, as the
    /// live session's provider requests produce between tool calls.
    fn budget(&mut self) {
        self.budget += 1;
        let status = RequestBudgetStatusV1 {
            used: self.budget,
            budget: RequestBudgetV1::default(),
            phase: RequestBudgetPhaseV1::Progress,
            continuation: RequestBudgetContinuationV1 {
                session_id: SessionId::new("tuivirt-session"),
                run_id: RunId::new("run-1"),
                branch_id: None,
                agent_id: None,
            },
        };
        apply(
            &mut self.model,
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(format!("budget-{}", self.budget)),
                item: TurnItem::Extension {
                    kind: PROVIDER_REQUEST_BUDGET_EXTENSION_KIND.into(),
                    data: serde_json::to_value(&status).unwrap(),
                },
            }),
        );
    }

    fn tool(
        &mut self,
        id: &str,
        name: &str,
        args: serde_json::Value,
        output: &str,
        outcome: (ToolStatus, BoundedResult),
    ) {
        let item = |status| TurnItem::ToolCall {
            call_id: format!("call-{id}"),
            name: name.to_owned(),
            args: args.clone(),
            status,
        };
        apply(
            &mut self.model,
            EventPayload::Item(ItemEvent::Started {
                item_id: ItemId::new(id),
                item: item(ToolStatus::InProgress),
            }),
        );
        if !output.is_empty() {
            apply(
                &mut self.model,
                EventPayload::Item(ItemEvent::Delta {
                    item_id: ItemId::new(id),
                    delta: ItemDelta::CommandOutput {
                        stream: OutputStream::Stdout,
                        chunk_b64: base64::engine::general_purpose::STANDARD
                            .encode(output.as_bytes()),
                    },
                }),
            );
        }
        apply(
            &mut self.model,
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(id),
                item: item(outcome.0),
            }),
        );
        apply(
            &mut self.model,
            EventPayload::ToolResult {
                call_id: format!("call-{id}"),
                result: outcome.1,
            },
        );
        self.budget();
    }

    fn write(&mut self, id: &str, path: &str, content: &str) {
        self.tool(
            id,
            "fs_write",
            serde_json::json!({ "path": path, "content": content }),
            "",
            ok(&format!("wrote {} bytes to {path}", content.len())),
        );
    }

    fn edit(&mut self, id: &str, path: &str, edits: serde_json::Value) {
        self.tool(
            id,
            "fs_edit",
            serde_json::json!({ "path": path, "edits": edits }),
            "",
            ok(&format!("edited {path}")),
        );
    }

    fn end_turn(&mut self, n: usize) {
        self.budget();
        push_agent(&mut self.model, &format!("reply-{n}"), &format!("T{n}DONE"));
    }
}

fn ok(preview: &str) -> (ToolStatus, BoundedResult) {
    (
        ToolStatus::Completed,
        result(preview, ToolResultStatus::Completed, None),
    )
}

/// The verifier's live session (`11-verify5-opus/live/fake-script.py`),
/// rebuilt through the projection: eleven turns, ten writes (one of 1,300
/// lines), multi-edits, a `replace_all`, a rejected call, folded
/// `list_tools`, reads, shells and a test run — plus `filler` extra turns
/// of agent prose to push the transcript far past the eager threshold.
fn live_session(filler: usize) -> AppModel {
    let mut b = Builder::new();
    push_user(&mut b.model, "turn 1");
    let big: String = (1..=1300).map(|i| format!("row {i:04}\n")).collect();
    b.write("w1", "braces.rs", "fn f() {\n}\nTODO\n");
    b.write("w2", "calls.rs", "foo(1);\nbar(1)\n");
    b.write("w3", "boundary.txt", "a\nb\nc\n");
    b.write("w4", "crlf.txt", "l1\r\nl2\r\nl3");
    b.write("w5", "uni.txt", "é😀\n\tz\n👨‍👩‍👧 q");
    b.write("w6", "all.txt", "x\ny\nx\nx\n");
    b.write("w7", "chain.txt", "A\nk\n");
    b.write("w8", "big.txt", &big);
    b.write(
        "w9",
        "data.json",
        "{\"output\":\"INNER_OUTPUT\",\"other\":\"KEEP_ME\",\"mutation_digest\":\"FILEDATA\"}\n",
    );
    b.write(
        "w10",
        "env.json",
        "{\"status\": \"completed\", \"effect_id\": \"effect-FAKE\", \"exit_code\": 0, \"output_bytes\": 9, \"command_arg_digest\": \"blake3:aa\", \"transcript_digest\": \"blake3:bb\", \"output\": \"UNWRAPPED_FROM_FILE\"}",
    );
    b.end_turn(1);
    let edits: [(&str, &str, serde_json::Value); 9] = [
        (
            "e1",
            "braces.rs",
            serde_json::json!([{"old": "TODO", "new": "}\n}"}]),
        ),
        (
            "e2",
            "calls.rs",
            serde_json::json!([{"old": "foo(1)", "new": "bar(1)"}, {"old": "1);", "new": "2);"}]),
        ),
        (
            "e3",
            "boundary.txt",
            serde_json::json!([{"old": "a\n", "new": "a"}, {"old": "c", "new": "C"}]),
        ),
        (
            "e4",
            "crlf.txt",
            serde_json::json!([{"old": "l3", "new": "L3\r\nL4"}, {"old": "l1", "new": "L1"}]),
        ),
        (
            "e5",
            "uni.txt",
            serde_json::json!([{"old": "q", "new": "Q\nR"}, {"old": "\tz", "new": "\tZ"}]),
        ),
        (
            "e6",
            "all.txt",
            serde_json::json!([{"old": "x", "new": "X", "replace_all": true}]),
        ),
        (
            "e7",
            "all.txt",
            serde_json::json!([{"old": "y", "new": "Y", "replace_all": true}]),
        ),
        (
            "e8",
            "chain.txt",
            serde_json::json!([{"old": "A", "new": "B"}, {"old": "B", "new": "C"}]),
        ),
        (
            "e9",
            "big.txt",
            serde_json::json!([{"old": "row 1150\n", "new": "ROW 1150\nextra\n"}, {"old": "row 0002\n", "new": "ROW 0002\nins\n"}]),
        ),
    ];
    // Turns 2-9: one edit each, except turn 7, which holds e6 and e7.
    let mut turn = 2;
    for (id, path, list) in edits {
        if id != "e7" {
            push_user(&mut b.model, &format!("turn {turn}"));
        }
        b.edit(id, path, list);
        if id != "e6" {
            b.end_turn(turn);
            turn += 1;
        }
    }
    push_user(&mut b.model, "turn 10");
    b.tool(
        "e10",
        "edit",
        serde_json::json!({"file_path": "calls.rs", "old_string": "bar(1)\n", "new_string": "baz(1)\n"}),
        "",
        (
            ToolStatus::Failed,
            result(
                "",
                ToolResultStatus::Failed,
                Some(
                    "Tool grant denied — This session is not allowed to use the requested tool. [grant-ceiling-violation] and a long tail that wraps",
                ),
            ),
        ),
    );
    b.end_turn(10);
    push_user(&mut b.model, "turn 11");
    let hint = "{\"hint\":\"These tools are now advertised for the rest of this session.\",\"tools\":[{\"description\":\"run one local thing\"}]}";
    b.tool(
        "lt1",
        "list_tools",
        serde_json::json!({"filter": "fs_path"}),
        "",
        ok(hint),
    );
    b.tool(
        "lt2",
        "list_tools",
        serde_json::json!({"filter": "test_run"}),
        "",
        ok(hint),
    );
    b.tool(
        "rd",
        "fs_read",
        serde_json::json!({"path": "data.json"}),
        "",
        ok("     1\t{\"output\":\"INNER_OUTPUT\"}\n"),
    );
    b.tool(
        "sh",
        "process_exec",
        serde_json::json!({"command": "cat env.json; echo; cat data.json"}),
        "{\"status\": \"completed\"}\n{\"output\":\"INNER_OUTPUT\"}\n",
        ok(""),
    );
    b.tool(
        "fp",
        "fs_path",
        serde_json::json!({"operation": "copy", "source": "calls.rs", "destination": "calls-copy.rs"}),
        "",
        ok("copied calls.rs to calls-copy.rs"),
    );
    b.tool(
        "re",
        "fs_read",
        serde_json::json!({"path": "env.json"}),
        "",
        ok("     1\t{\"status\": \"completed\"}\n"),
    );
    b.tool(
        "tr",
        "test_run",
        serde_json::json!({"command": "printf 'running 2 tests\\ntest a ... ok\\ntest b ... ok\\n'"}),
        "running 2 tests\ntest a ... ok\ntest b ... ok\n",
        ok(""),
    );
    b.tool(
        "sq",
        "process_exec",
        serde_json::json!({"command": "true"}),
        "",
        ok(""),
    );
    b.end_turn(11);
    for n in 0..filler {
        push_user(&mut b.model, &format!("filler turn {n}"));
        b.write(
            &format!("fw{n}"),
            &format!("f{n}.txt"),
            &"filler line\n".repeat(1 + n % 7),
        );
        b.end_turn(100 + n);
    }
    b.model
}

/// The frame after an action, as the live runtime paints it: ONCE. A second
/// paint of the unchanged model must be identical (a frame is a function of
/// the model — verify 5's live captures were first paints, and a paint that
/// only converged on the NEXT frame came back different after Esc).
fn painted(model: &AppModel, width: u16, height: u16, what: &str, tally: &mut Tally) -> Snapshot {
    let first = draw(model, width, height);
    let again = draw(model, width, height);
    if again.dump("f") != first.dump("f") {
        tally.failures.push(format!(
            "{what}: repainting the unchanged model changed the frame: {}",
            diff_of(&first, &again)
        ));
    }
    first
}

/// First-match hit at a cell, as the runtime's `hit_at` resolves a click.
fn hit_at(frame: &Snapshot, x: u16, y: u16) -> Option<Hit> {
    frame
        .hits
        .iter()
        .find(|(rect, _)| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        })
        .map(|(_, hit)| hit.clone())
}

fn door_rows(frame: &Snapshot) -> Vec<u16> {
    let area = frame.transcript;
    (area.y..area.y + area.height)
        .filter(|&y| {
            frame
                .rows
                .get(usize::from(y))
                .is_some_and(|row| row.trim_start().starts_with('⎿'))
        })
        .collect()
}

fn diff_of(before: &Snapshot, after: &Snapshot) -> String {
    let a = before.dump("f");
    let b = after.dump("f");
    let first = a
        .lines()
        .zip(b.lines())
        .enumerate()
        .find(|(_, (x, y))| x != y)
        .map_or_else(
            || "line counts differ".to_owned(),
            |(line, (x, y))| format!("golden line {}:\n  before: {x}\n  after:  {y}", line + 1),
        );
    format!(
        "{first}\n--- before ---\n{}\n--- after ---\n{}",
        before.rows.join("\n"),
        after.rows.join("\n")
    )
}

#[derive(Default, Debug)]
struct Tally {
    positions: usize,
    doors: usize,
    opened: usize,
    identical: usize,
    failures: Vec<String>,
}

fn closer(model: &mut AppModel, n: usize) -> &'static str {
    match n % 5 {
        0 => {
            model.handle(key(KeyCode::Esc));
            "Esc"
        }
        1 => {
            model.handle(ctrl('o'));
            "⌃O"
        }
        2 => {
            model.handle(key(KeyCode::Char('q')));
            "q"
        }
        3 => {
            model.handle(key(KeyCode::Enter));
            "⏎"
        }
        _ => {
            model.handle_hit(Hit::ToolDetailClose);
            "esc-back click"
        }
    }
}

/// A fresh live-shaped session scrolled `notches` wheel notches up from the
/// bottom, one frame per notch as the runtime paints them.
fn scrolled(filler: usize, width: u16, height: u16, notches: usize) -> AppModel {
    let model = live_session(filler);
    draw(&model, width, height);
    // The attach-time no-op wheel-down the live repro sends first.
    let mut model = model;
    model.handle_wheel(false);
    draw(&model, width, height);
    for _ in 0..notches {
        model.handle_wheel(true);
        draw(&model, width, height);
    }
    model
}

/// Click every drawn `⎿` line at one scroll position and require the
/// identical frame back after each close.
fn click_doors(model: &mut AppModel, what: &str, size: (u16, u16), tally: &mut Tally) {
    let (width, height) = size;
    let before = painted(model, width, height, what, tally);
    tally.positions += 1;
    for y in door_rows(&before) {
        tally.doors += 1;
        let hit = hit_at(&before, 8, y);
        let Some(Hit::ToolDetail(id)) = hit.clone() else {
            tally.failures.push(format!(
                "{what} row={y}: the ⎿ line `{}` resolves to {hit:?}, not a door",
                before.rows[usize::from(y)].trim()
            ));
            continue;
        };
        model.handle_hit(Hit::ToolDetail(id.clone()));
        let view = draw(model, width, height);
        if !view.contains("full detail") {
            tally
                .failures
                .push(format!("{what} row={y}: door {id} opened nothing"));
            continue;
        }
        tally.opened += 1;
        let how = closer(model, tally.opened);
        let after = painted(model, width, height, what, tally);
        if after.dump("f") == before.dump("f") {
            tally.identical += 1;
        } else {
            tally.failures.push(format!(
                "{what} row={y}: click {id} then {how} did not return the identical frame: {}",
                diff_of(&before, &after)
            ));
            return;
        }
    }
}

/// EVERY scroll offset from the bottom to the top, walked one row (and one
/// painted frame) at a time on ONE model — the one-row drag-autoscroll
/// gesture, so no offset is skipped.
fn click_sweep(filler: usize, width: u16, height: u16) -> Tally {
    let mut tally = Tally::default();
    let mut model = scrolled(filler, width, height, 0);
    let mut step = 0usize;
    loop {
        let what = format!(
            "{width}x{height} filler={filler} offset={}",
            model.scroll_back.get()
        );
        click_doors(&mut model, &what, (width, height), &mut tally);
        assert!(step < 20_000, "{what}: the walk never reached the top");
        if model.scroll_back.get() >= model.scroll_max.get() {
            break;
        }
        model.drag_autoscroll(true);
        draw(&model, width, height);
        step += 1;
    }
    // The verifier's own positions, each from a FRESH model (a relaunched
    // TUI) scrolled up that many notches.
    for notches in [0usize, 4, 10, 20, 45, 90] {
        let mut model = scrolled(filler, width, height, notches);
        let what = format!("{width}x{height} filler={filler} fresh notches={notches}");
        click_doors(&mut model, &what, (width, height), &mut tally);
    }
    tally
}

/// Focus every header on screen (as a click on it, ⌥N/⌥P or `/collapse`
/// does), open with ⌃O, close, and require the identical frame back.
fn keyboard_doors(model: &mut AppModel, what: &str, size: (u16, u16), tally: &mut Tally) {
    let (width, height) = size;
    let first = painted(model, width, height, what, tally);
    tally.positions += 1;
    let ids: Vec<String> = first
        .hits
        .iter()
        .filter_map(|(_, hit)| match hit {
            Hit::ToolRowToggle(id) => Some(id.clone()),
            _ => None,
        })
        .collect();
    for id in ids {
        model.toolfold.set_focus(Some(&id));
        let before = painted(model, width, height, what, tally);
        tally.doors += 1;
        model.handle(ctrl('o'));
        let view = draw(model, width, height);
        if !view.contains("full detail") {
            tally
                .failures
                .push(format!("{what}: ⌃O on focused {id} opened nothing"));
            continue;
        }
        tally.opened += 1;
        let how = closer(model, tally.opened);
        let after = painted(model, width, height, what, tally);
        if after.dump("f") == before.dump("f") {
            tally.identical += 1;
        } else {
            tally.failures.push(format!(
                "{what}: focus {id}, ⌃O, {how} did not return the identical frame: {}",
                diff_of(&before, &after)
            ));
            return;
        }
    }
}

fn keyboard_sweep(filler: usize, width: u16, height: u16) -> Tally {
    let mut tally = Tally::default();
    let mut model = scrolled(filler, width, height, 0);
    let mut notch = 0usize;
    loop {
        let what = format!("{width}x{height} filler={filler} walk notch={notch}");
        keyboard_doors(&mut model, &what, (width, height), &mut tally);
        // Release the focus in its own frame (a re-layout the content
        // anchor holds), then take the next notch.
        model.toolfold.set_focus(None);
        draw(&model, width, height);
        if model.scroll_back.get() >= model.scroll_max.get() {
            break;
        }
        assert!(notch < 2_000, "{what}: the walk never reached the top");
        model.handle_wheel(true);
        draw(&model, width, height);
        notch += 1;
    }
    for notches in [0usize, 4, 10, 20, 45, 90] {
        let mut model = scrolled(filler, width, height, notches);
        let what = format!("{width}x{height} filler={filler} fresh notches={notches}");
        keyboard_doors(&mut model, &what, (width, height), &mut tally);
    }
    tally
}

fn report(name: &str, tally: &Tally) {
    println!(
        "{name}: positions={} doors={} opened={} identical={} failures={}",
        tally.positions,
        tally.doors,
        tally.opened,
        tally.identical,
        tally.failures.len()
    );
    for failure in tally.failures.iter().take(3) {
        println!("  {failure}");
    }
}

#[test]
fn a_door_click_returns_the_identical_frame_at_every_scroll_offset() {
    let mut failures = 0;
    for (width, height) in [(118u16, 36u16), (80, 24)] {
        for filler in [0usize, 40] {
            let tally = click_sweep(filler, width, height);
            report(&format!("click {width}x{height} filler={filler}"), &tally);
            assert!(tally.doors > 0 && tally.positions > 3, "{tally:?}");
            failures += tally.failures.len();
        }
    }
    assert_eq!(failures, 0, "see the report above");
}

#[test]
fn keyboard_focus_and_ctrl_o_return_the_identical_frame_at_every_scroll_offset() {
    let mut failures = 0;
    for (width, height) in [(118u16, 36u16), (80, 24)] {
        for filler in [0usize, 40] {
            let tally = keyboard_sweep(filler, width, height);
            report(
                &format!("keyboard {width}x{height} filler={filler}"),
                &tally,
            );
            assert!(tally.doors > 0 && tally.positions > 3, "{tally:?}");
            failures += tally.failures.len();
        }
    }
    assert_eq!(failures, 0, "see the report above");
}

/// Verify 5's keyboard path, exactly: at the TOP of the transcript,
/// `/collapse prev` (⌥P) focuses the newest row and reveals it, ⌃O opens
/// its detail, and each closer returns the frame the reveal painted.
#[test]
fn collapse_prev_from_the_top_then_ctrl_o_and_back_is_identical() {
    for (width, height) in [(118u16, 36u16), (80, 24)] {
        for filler in [0usize, 40] {
            for close in 0..5 {
                let mut model = scrolled(filler, width, height, 400);
                let mut tally = Tally::default();
                let what = format!("{width}x{height} filler={filler}");
                let top = painted(&model, width, height, &what, &mut tally);
                assert!(model.scroll_back.get() > 0, "{}", top.rows.join("\n"));
                model.move_tool_focus(false);
                let before = painted(&model, width, height, &what, &mut tally);
                model.handle(ctrl('o'));
                let view = draw(&model, width, height);
                assert!(view.contains("full detail"), "{}", view.rows.join("\n"));
                let how = closer(&mut model, close);
                let after = painted(&model, width, height, &what, &mut tally);
                assert!(tally.failures.is_empty(), "{:#?}", tally.failures);
                assert!(
                    after.dump("f") == before.dump("f"),
                    "{what}: ⌥P, ⌃O, {how}: {}",
                    diff_of(&before, &after)
                );
            }
        }
    }
}

/// The detail view saves the CONTENT position and restores it: even when
/// the transcript under it is re-measured while it is open (a disclosure
/// change re-seeds every height estimate), Esc puts the same rows back on
/// screen.
#[test]
fn closing_restores_the_content_position_across_a_relayout_underneath() {
    for (width, height) in [(118u16, 36u16), (80, 24)] {
        for notches in [3usize, 20, 45, 90, 400] {
            let mut model = scrolled(40, width, height, notches);
            let mut tally = Tally::default();
            let what = format!("{width}x{height} notches={notches}");
            let before = painted(&model, width, height, &what, &mut tally);
            let Some((_, Hit::ToolDetail(id))) = before
                .hits
                .iter()
                .find(|(_, hit)| matches!(hit, Hit::ToolDetail(_)))
                .cloned()
            else {
                continue;
            };
            model.handle_hit(Hit::ToolDetail(id));
            draw(&model, width, height);
            // Re-measure everything underneath while the view is open: a
            // blanket expand and back (two disclosure revisions).
            model.toggle_all_tool_rows();
            draw(&model, width, height);
            model.toggle_all_tool_rows();
            draw(&model, width, height);
            model.handle(key(KeyCode::Esc));
            let after = painted(&model, width, height, &what, &mut tally);
            assert!(tally.failures.is_empty(), "{:#?}", tally.failures);
            assert!(
                after.transcript_interior() == before.transcript_interior(),
                "{what}: {}",
                diff_of(&before, &after)
            );
        }
    }
}
