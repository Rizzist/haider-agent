//! 973-tui-toolview — tool rows in the Claude Code idiom, on real frames.
//!
//! Reference: `state/reference/claude-code-tool-rows-write-diff.png` —
//! `● Write(path)` over `⎿ Added 46 lines, removed 74 lines` and a red/green
//! numbered diff preview; `Read 1 file`; `Ran 3 shell commands`; a failed
//! write as a red `⎿ Error writing file`. These pins cover the collapsed
//! rows (goldens at 80×24 and 118×36, dark and light), the full-detail view
//! (⌃O on a focused row, or a click on the `(⌃O to expand)` door) and its
//! exact return (Esc/⌃O: same frame, same scroll), the family folds, the
//! hidden request-budget tally, and edit line anchors.
//!
//! Regenerate ONLY deliberately: `UPDATE_TUIVIRT_GOLDENS=1 cargo test -p
//! haider-tui --test tuivirt_toolview_tests`, then review the fixture diff.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::ids::{ItemId, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_protocol::request_budget::{
    PROVIDER_REQUEST_BUDGET_EXTENSION_KIND, RequestBudgetContinuationV1, RequestBudgetPhaseV1,
    RequestBudgetStatusV1, RequestBudgetV1,
};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_tui::app::{AppEvent, AppModel, Hit};
use haider_tui::theme::ThemeKey;
use haider_tui::toolfold::ToolTiming;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod tuivirt_common;
use tuivirt_common::{apply, check_golden, draw, push_agent, push_user, session_model};

const SIZES: [(u16, u16); 2] = [(80, 24), (118, 36)];
const WORKSPACE: &str = "/work/haider";

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

fn tool(id: &str, name: &str, args: serde_json::Value, status: ToolStatus) -> TurnItem {
    TurnItem::ToolCall {
        call_id: format!("call-{id}"),
        name: name.to_owned(),
        args,
        status,
    }
}

/// A tool call driven like a real stream: `Started`, optional output,
/// `Completed`, then the joined `ToolResult`.
fn push_tool(
    model: &mut AppModel,
    id: &str,
    name: &str,
    args: serde_json::Value,
    output: &str,
    outcome: (ToolStatus, BoundedResult),
) {
    apply(
        model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new(id),
            item: tool(id, name, args.clone(), ToolStatus::InProgress),
        }),
    );
    if !output.is_empty() {
        apply(model, output_delta(id, output));
    }
    apply(
        model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item: tool(id, name, args, outcome.0),
        }),
    );
    apply(
        model,
        EventPayload::ToolResult {
            call_id: format!("call-{id}"),
            result: outcome.1,
        },
    );
}

fn ok(preview: &str) -> (ToolStatus, BoundedResult) {
    (
        ToolStatus::Completed,
        result(preview, ToolResultStatus::Completed, None),
    )
}

fn output_delta(id: &str, text: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new(id),
        delta: ItemDelta::CommandOutput {
            stream: OutputStream::Stdout,
            chunk_b64: base64::engine::general_purpose::STANDARD.encode(text.as_bytes()),
        },
    })
}

fn push_command(model: &mut AppModel, id: &str, command: &str, output: &str, exit: i32) {
    let item = |status, exit_code| TurnItem::CommandExecution {
        call_id: format!("call-{id}"),
        command: command.to_owned(),
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
    if !output.is_empty() {
        apply(model, output_delta(id, output));
    }
    apply(
        model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item: item(
                if exit == 0 {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Failed
                },
                Some(exit),
            ),
        }),
    );
}

fn lanes_content() -> String {
    let mut text = String::from("# 973 lanes — RE-SCOPED by owner 2026-09-23\n\n");
    for (n, lane) in [
        "973-tui-toolview",
        "973-request-budget",
        "973-catalog-daemon",
        "973-mobile-oauth",
        "973-bench",
    ]
    .iter()
    .enumerate()
    {
        text.push_str(&format!("| {} | {lane} | implement | ⏳ |\n", n + 1));
    }
    text
}

fn budget_progress() -> EventPayload {
    let status = RequestBudgetStatusV1 {
        used: 6,
        budget: RequestBudgetV1::default(),
        phase: RequestBudgetPhaseV1::Progress,
        continuation: RequestBudgetContinuationV1 {
            session_id: SessionId::new("tuivirt-session"),
            run_id: RunId::new("run-1"),
            branch_id: None,
            agent_id: None,
        },
    };
    EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("budget-6"),
        item: TurnItem::Extension {
            kind: PROVIDER_REQUEST_BUDGET_EXTENSION_KIND.into(),
            data: serde_json::to_value(&status).unwrap(),
        },
    })
}

/// The owner's transcript, in the new idiom: a write with its diff, an
/// edit with a numbered diff, three folded reads, a heredoc shell call, a
/// failing command, a malformed-JSON write, and the budget tally that must
/// no longer render.
fn toolview_model() -> AppModel {
    let mut model = session_model();
    model.cwd = WORKSPACE.to_owned();
    model.clock_ms = 1_700_000_000_000;
    push_user(&mut model, "re-scope the 973 lanes and log the experience");
    push_agent(&mut model, "reply-1", "Updating the lane table first.");
    push_tool(
        &mut model,
        "write-lanes",
        "fs_write",
        serde_json::json!({
            "path": format!("{WORKSPACE}/state/973-LANES.md"),
            "content": lanes_content(),
        }),
        "",
        ok("wrote 312 bytes to state/973-LANES.md"),
    );
    push_tool(
        &mut model,
        "edit-status",
        "fs_edit",
        serde_json::json!({
            "path": format!("{WORKSPACE}/state/970-STATUS.md"),
            "edits": [{
                "old": "## Stage\nplanning (not started; 972 release chain first)\n## Owner",
                "new": "## Stage\nRE-SCOPED by owner 2026-09-23\nimplementation under way\n## Owner",
            }],
        }),
        "",
        ok("edited state/970-STATUS.md (1 replacement)"),
    );
    model
        .edit_anchors
        .insert("edit-status".to_owned(), vec![Some(41)]);
    model.edit_anchor_revision += 1;
    apply(&mut model, budget_progress());
    for (n, file) in ["src/app.rs", "src/render.rs", "src/toolfold.rs"]
        .iter()
        .enumerate()
    {
        push_tool(
            &mut model,
            &format!("read-{n}"),
            "fs_read",
            serde_json::json!({ "path": format!("{WORKSPACE}/crates/haider-tui/{file}") }),
            "",
            ok(&(1..=120)
                .map(|line| format!("{line:>5}\tline {line}\n"))
                .collect::<String>()),
        );
    }
    push_tool(
        &mut model,
        "log",
        "process_exec",
        serde_json::json!({
            "command": "cat >> experience-log.jsonl <<'EOF'\n{\"t\":\"2026-09-23T20:01:00Z\",\"tool\":\"fs_write\",\"ok\":true}\nEOF\nwc -l experience-log.jsonl",
        }),
        "42 experience-log.jsonl\n",
        ok(""),
    );
    push_command(
        &mut model,
        "gate",
        "cargo test -p haider-tui --locked",
        "running 1636 tests\ntest render::golden ... FAILED\nerror: test failed, to rerun pass `-p haider-tui --lib`\n",
        101,
    );
    push_tool(
        &mut model,
        "bad-write",
        "fs_write",
        serde_json::json!({}),
        "",
        (
            ToolStatus::Failed,
            result(
                "",
                ToolResultStatus::Failed,
                Some(
                    "Invalid tool call — tool call `toolu_01` ended with malformed JSON arguments: expected `,` or `}` at line 1 column 4097",
                ),
            ),
        ),
    );
    model.tool_timings.insert(
        "gate".to_owned(),
        ToolTiming {
            started_ms: model.clock_ms - 8_200,
            ended_ms: Some(model.clock_ms),
        },
    );
    model
}

// ---- 1. goldens -----------------------------------------------------------

#[test]
fn claude_code_style_rows_pin_at_both_sizes_in_dark_and_light() {
    for key in [ThemeKey::Dark, ThemeKey::Light] {
        let mut model = toolview_model();
        model.theme = key;
        for (width, height) in SIZES {
            check_golden(
                &format!("toolview_rows_{}", key.name()),
                &draw(&model, width, height),
            );
        }
    }
}

#[test]
fn the_full_detail_view_pins_at_both_sizes() {
    let mut model = toolview_model();
    model.theme = ThemeKey::Dark;
    model.toolfold.set_focus(Some("edit-status"));
    model.handle(ctrl('o'));
    assert!(model.tool_detail.is_some());
    for (width, height) in SIZES {
        check_golden("toolview_detail_edit", &draw(&model, width, height));
    }
    model.handle(key(KeyCode::Esc));
    model.toolfold.set_focus(Some("log"));
    model.handle(ctrl('o'));
    for (width, height) in SIZES {
        check_golden("toolview_detail_shell", &draw(&model, width, height));
    }
}

// ---- 2. the collapsed rows' content --------------------------------------

#[test]
fn the_collapsed_rows_speak_the_reference_vocabulary() {
    let model = toolview_model();
    let frame = draw(&model, 118, 36);
    for needle in [
        "● Write(state/973-LANES.md)",
        "⎿ Wrote 7 lines",
        "(⌃O to expand)",
        "● Edit(state/970-STATUS.md)",
        "⎿ Added 2 lines, removed 1 line",
        "41   ## Stage",
        "42 - planning (not started; 972 release chain first)",
        "42 + RE-SCOPED by owner 2026-09-23",
        "Read 3 files",
        "● Bash(cat >> experience-log.jsonl <<'EOF' …)",
        "⎿ 42 experience-log.jsonl",
        "● Bash(cargo test -p haider-tui --locked) · 8s",
        "⎿ Exit code 101 · running 1636 tests",
        "● Write",
        "⎿ Error writing file · Invalid tool call",
    ] {
        assert!(
            frame.contains(needle),
            "missing {needle:?}:\n{}",
            frame.rows.join("\n")
        );
    }
    // The folded reads stand alone: the first line of a file is not a result.
    let reads = frame.row_containing("Read 3 files").unwrap();
    assert!(
        !frame.rows[reads + 1].contains('⎿'),
        "{}",
        frame.rows[reads + 1]
    );
    // The request tally is gone from the transcript.
    assert!(!frame.contains("tranche"));
    assert!(!frame.contains("— in progress"));
    // The heredoc body never reaches the transcript collapsed.
    assert!(!frame.contains("2026-09-23T20:01:00Z"));
}

#[test]
fn diff_rows_wear_their_grounds_and_the_failure_its_red() {
    let model = toolview_model();
    let theme = model.theme.theme();
    let frame = draw(&model, 118, 36);
    let removed = frame.row_containing("42 - planning").expect("removed row");
    let added = frame.row_containing("42 + RE-SCOPED").expect("added row");
    let ground = |row: usize| frame.cells[row][20].1.bg;
    assert_eq!(ground(removed), theme.diff_removed_ground().into());
    assert_eq!(ground(added), theme.diff_added_ground().into());
    // The ground spans the row, as the reference's does.
    assert_eq!(
        frame.cells[added][100].1.bg,
        theme.diff_added_ground().into()
    );
    let failed = frame
        .row_containing("⎿ Error writing file")
        .expect("failure line");
    let column = frame.rows[failed].find("Error").unwrap();
    let column = frame.rows[failed][..column].chars().count();
    assert_eq!(frame.cells[failed][column].1.fg, theme.err.into());
    // Its header bullet is red too.
    let header = failed - 1;
    let bullet = frame.rows[header].chars().position(|c| c == '●').unwrap();
    assert_eq!(frame.cells[header][bullet].1.fg, theme.err.into());
}

#[test]
fn at_80_columns_every_header_keeps_its_verb() {
    let model = toolview_model();
    for width in [80u16, 60, 40] {
        let frame = draw(&model, width, 60);
        for verb in ["● Write(", "● Edit(", "● Bash(", "Read 3 files"] {
            assert!(
                frame.contains(verb),
                "{verb} at {width} cols:\n{}",
                frame.rows.join("\n")
            );
        }
    }
}

#[test]
fn consecutive_writes_never_fold_and_quiet_mode_keeps_the_outcome_on_the_header() {
    let mut model = session_model();
    for n in 0..2 {
        push_tool(
            &mut model,
            &format!("w{n}"),
            "fs_write",
            serde_json::json!({"path": format!("f{n}.md"), "content": "x\n"}),
            "",
            ok("wrote 2 bytes"),
        );
    }
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("● Write(f0.md)") && frame.contains("● Write(f1.md)"));
    push_command(&mut model, "c", "false", "", 1);
    model.set_tool_verbosity(haider_tui::toolfold::Verbosity::Quiet);
    let frame = draw(&model, 118, 36);
    assert!(!frame.contains("⎿"), "quiet draws no result lines");
    assert!(
        frame.contains("● Bash(false) · exit 1"),
        "with no ⎿ line the exit code stays on the header:\n{}",
        frame.rows.join("\n")
    );
}

// ---- 3. the full-detail view: open, read, scroll, return ----------------

/// A long transcript scrolled back, a focused tool row, ⌃O, Esc: the frame
/// after Esc is the frame before ⌃O, and `scroll_back` never moved.
#[test]
fn ctrl_o_opens_the_full_detail_and_esc_returns_to_the_same_place() {
    let mut model = toolview_model();
    for n in 0..30 {
        push_agent(
            &mut model,
            &format!("tail-{n}"),
            &tuivirt_common::agent_row(n),
        );
    }
    draw(&model, 118, 36);
    model.scroll_back.set(40);
    model.toolfold.set_focus(Some("log"));
    let before = draw(&model, 118, 36);
    let scroll_before = model.scroll_back.get();
    assert!(scroll_before > 0, "the transcript is really scrolled back");

    model.handle(ctrl('o'));
    let view = draw(&model, 118, 36);
    assert!(view.contains("full detail"), "{}", view.rows.join("\n"));
    assert!(view.contains("Arguments"));
    assert!(
        view.contains("{\"t\":\"2026-09-23T20:01:00Z\",\"tool\":\"fs_write\",\"ok\":true}"),
        "the whole heredoc is one gesture away:\n{}",
        view.rows.join("\n")
    );
    assert!(view.contains("42 experience-log.jsonl"));
    assert_eq!(
        model.scroll_back.get(),
        scroll_before,
        "opening never scrolls"
    );

    model.handle(key(KeyCode::Esc));
    assert!(model.tool_detail.is_none());
    let after = draw(&model, 118, 36);
    assert_eq!(model.scroll_back.get(), scroll_before);
    assert_eq!(before.rows, after.rows, "Esc lands exactly where ⌃O left");

    // ⌃O closes it too — "the same key collapses".
    model.handle(ctrl('o'));
    assert!(model.tool_detail.is_some());
    model.handle(ctrl('o'));
    assert!(model.tool_detail.is_none());
    assert_eq!(draw(&model, 118, 36).rows, before.rows);
}

#[test]
fn ctrl_o_without_a_focused_row_keeps_its_expand_every_row_meaning() {
    let mut model = toolview_model();
    assert!(model.toolfold.focus().is_none());
    model.handle(ctrl('o'));
    assert!(model.tool_detail.is_none());
    assert!(
        model.toolfold.all_expanded(),
        "⌃O stays ⌥T's twin (owner ruling 4)"
    );
}

#[test]
fn the_detail_view_scrolls_itself_and_owns_the_keys() {
    let mut model = toolview_model();
    push_tool(
        &mut model,
        "long",
        "process_exec",
        serde_json::json!({"command": "seq 1 200"}),
        &(1..=200).map(|n| format!("{n}\n")).collect::<String>(),
        ok(""),
    );
    draw(&model, 80, 24);
    let scroll_back = model.scroll_back.get();
    model.toolfold.set_focus(Some("long"));
    model.handle(ctrl('o'));
    let first = draw(&model, 80, 24);
    assert!(first.contains("rows 1–"), "{}", first.rows.join("\n"));
    model.handle_wheel(false);
    let view = model.tool_detail.as_ref().unwrap();
    assert_eq!(view.scroll, 3, "the wheel moves the view");
    assert_eq!(
        model.scroll_back.get(),
        scroll_back,
        "…never the transcript"
    );
    model.handle(key(KeyCode::PageDown));
    model.handle(key(KeyCode::Char('j')));
    let scrolled = model.tool_detail.as_ref().unwrap().scroll;
    assert!(scrolled > 4, "{scrolled}");
    model.handle(key(KeyCode::End));
    draw(&model, 80, 24);
    let end = draw(&model, 80, 24);
    assert!(
        end.contains("      200"),
        "End reaches the last row:\n{}",
        end.rows.join("\n")
    );
    model.handle(key(KeyCode::Home));
    assert_eq!(model.tool_detail.as_ref().unwrap().scroll, 0);
    // Printable keys never leak into the composer underneath.
    model.handle(key(KeyCode::Char('x')));
    assert!(model.composer.text().is_empty());
    model.handle(key(KeyCode::Char('q')));
    assert!(model.tool_detail.is_none(), "q closes");
    model.handle(ctrl('o'));
    model.handle(key(KeyCode::Enter));
    assert!(model.tool_detail.is_none(), "⏎ closes");
}

#[test]
fn the_expand_door_is_clickable_and_the_view_owns_the_pointer() {
    let mut model = toolview_model();
    let frame = draw(&model, 118, 36);
    let (_, hit) = frame
        .find_hit(|hit| matches!(hit, Hit::ToolDetail(id) if id == "write-lanes"))
        .expect("the write's `(⌃O to expand)` door is a click target");
    model.handle_hit(hit);
    assert_eq!(
        model.tool_detail.as_ref().map(|view| view.item_id.as_str()),
        Some("write-lanes")
    );
    let view = draw(&model, 118, 36);
    assert!(
        view.contains("Content · 7 lines"),
        "{}",
        view.rows.join("\n")
    );
    assert!(view.contains("| 5 | 973-bench | implement | ⏳ |"));
    assert!(
        view.hits.iter().all(|(_, hit)| !matches!(
            hit,
            Hit::ToolRowToggle(_)
                | Hit::ToolFoldToggle(_)
                | Hit::ToolShowAll(_)
                | Hit::ToolDetail(_)
        )),
        "the covered transcript's targets are gone: {:?}",
        view.hits
    );
    assert!(view.has_hit(|hit| *hit == Hit::ToolDetailClose));
    // A stale transcript hit acts on nothing while the view is open.
    model.handle_hit(Hit::ToolRowToggle("log".to_owned()));
    assert!(model.tool_detail.is_some());
    model.handle_hit(Hit::ToolDetailClose);
    assert!(model.tool_detail.is_none());
    // A click can only open a row of the VIEWED transcript.
    assert!(!model.open_tool_detail("no-such-row"));
}

#[test]
fn a_detail_view_whose_row_vanished_says_so() {
    let mut model = toolview_model();
    assert!(model.open_tool_detail("log"));
    model.projection = haider_tui::projection::SessionProjection::default();
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("no longer in the transcript"));
}

#[test]
fn a_failed_calls_detail_carries_its_unshortened_reason() {
    let mut model = toolview_model();
    let long = format!("fatal: {}", "detail ".repeat(60));
    push_tool(
        &mut model,
        "fail-long",
        "web_fetch",
        serde_json::json!({"url": "https://example.test"}),
        "",
        (
            ToolStatus::Failed,
            result("", ToolResultStatus::Failed, Some(&long)),
        ),
    );
    model.toolfold.set_focus(Some("fail-long"));
    model.handle(ctrl('o'));
    let frame = draw(&model, 118, 60);
    let heading = frame
        .rows
        .iter()
        .position(|row| row.trim() == "Reason")
        .expect("the Reason section");
    let shown: String = frame.rows[heading + 1..]
        .iter()
        .flat_map(|row| row.split_whitespace())
        .collect();
    let full: String = long.split_whitespace().collect();
    assert!(
        shown.contains(&full),
        "every character of the reason is on screen, not the 240-char bound"
    );
    assert!(long.chars().count() > 240);
}

// ---- 4. edit anchors -----------------------------------------------------

#[test]
fn a_watched_edit_is_numbered_from_its_file_and_a_replayed_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.md");
    std::fs::write(&file, "one\ntwo\nthree\nFOUR\nfive\n").unwrap();
    let mut model = session_model();
    model.cwd = dir.path().display().to_string();
    model.clock_ms = 1_700_000_000_000;
    let args = serde_json::json!({
        "path": "notes.md",
        "edits": [{"old": "four", "new": "FOUR"}],
    });
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("e1"),
            item: tool("e1", "fs_edit", args.clone(), ToolStatus::InProgress),
        }),
    );
    model.note_tool_timings();
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("e1"),
            item: tool("e1", "fs_edit", args.clone(), ToolStatus::Completed),
        }),
    );
    model.clock_ms += 20;
    model.note_tool_timings();
    let revision = model.edit_anchor_revision;
    model.note_edit_anchors();
    assert_eq!(model.edit_anchors.get("e1"), Some(&vec![Some(4)]));
    assert_ne!(model.edit_anchor_revision, revision);
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("4 - four"), "{}", frame.rows.join("\n"));
    assert!(frame.contains("4 + FOUR"));

    // A row met already settled (a replay) is never located: the file may
    // have moved on since, so it carries no numbers at all.
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("e2"),
            item: tool("e2", "fs_edit", args, ToolStatus::Completed),
        }),
    );
    model.note_tool_timings();
    model.note_edit_anchors();
    assert!(!model.edit_anchors.contains_key("e2"));
    // A file the client cannot read yet (a workspace still being learned,
    // a remote daemon) records nothing, so a later beat can still place it.
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("e3"),
            item: tool(
                "e3",
                "fs_edit",
                serde_json::json!({"path": "later.md", "edits": [{"old": "a", "new": "b"}]}),
                ToolStatus::InProgress,
            ),
        }),
    );
    model.note_tool_timings();
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("e3"),
            item: tool(
                "e3",
                "fs_edit",
                serde_json::json!({"path": "later.md", "edits": [{"old": "a", "new": "b"}]}),
                ToolStatus::Completed,
            ),
        }),
    );
    model.note_tool_timings();
    model.note_edit_anchors();
    assert!(!model.edit_anchors.contains_key("e3"), "not readable yet");
    std::fs::write(
        dir.path().join("later.md"),
        "x
b
",
    )
    .unwrap();
    model.note_edit_anchors();
    assert_eq!(model.edit_anchors.get("e3"), Some(&vec![Some(2)]));
    // Resolving is once per row: a later beat does not re-read.
    let revision = model.edit_anchor_revision;
    model.note_edit_anchors();
    assert_eq!(model.edit_anchor_revision, revision);
}

// ---- 5. click geometry under the "Opened from" origin line -------------

/// A live session opened from another directory draws a (wrapping) origin
/// line ahead of entry 0. Every tool click target must sit on the row its
/// glyphs were drawn on — the rows below the origin line, not above them.
#[test]
fn tool_click_targets_sit_on_their_rows_below_the_origin_line() {
    let mut model = toolview_model();
    model.launch_origin = Some((
        1,
        Some("/private/tmp/a-rather-long-origin-directory/that/wraps/at/eighty/columns/ws".into()),
    ));
    for (width, height) in [(118u16, 40u16), (80, 40)] {
        let frame = draw(&model, width, height);
        assert!(frame.contains("Opened from"), "{}", frame.rows.join("\n"));
        let door = frame
            .row_containing("(⌃O to expand)")
            .expect("the write's door is drawn");
        let (rect, _) = frame
            .find_hit(|hit| matches!(hit, Hit::ToolDetail(id) if id == "write-lanes"))
            .expect("…and is a click target");
        assert_eq!(usize::from(rect.y), door, "door hit at {width} cols");
        let header = frame
            .row_containing("● Edit(state/970-STATUS.md)")
            .expect("edit header drawn");
        let (rect, _) = frame
            .find_hit(|hit| matches!(hit, Hit::ToolRowToggle(id) if id == "edit-status"))
            .expect("edit header is a click target");
        assert_eq!(usize::from(rect.y), header, "header hit at {width} cols");
    }
}
