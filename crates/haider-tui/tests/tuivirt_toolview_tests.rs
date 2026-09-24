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
    push_owner_tools(&mut model);
    model
}

/// The owner's tool calls (write · edit · reads · heredoc · failing gate ·
/// malformed write · budget tally), pushed onto `model`.
fn push_owner_tools(model: &mut AppModel) {
    push_tool(
        model,
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
        model,
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
    apply(model, budget_progress());
    for (n, file) in ["src/app.rs", "src/render.rs", "src/toolfold.rs"]
        .iter()
        .enumerate()
    {
        push_tool(
            model,
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
        model,
        "log",
        "process_exec",
        serde_json::json!({
            "command": "cat >> experience-log.jsonl <<'EOF'\n{\"t\":\"2026-09-23T20:01:00Z\",\"tool\":\"fs_write\",\"ok\":true}\nEOF\nwc -l experience-log.jsonl",
        }),
        "42 experience-log.jsonl\n",
        ok(""),
    );
    push_command(
        model,
        "gate",
        "cargo test -p haider-tui --locked",
        "running 1636 tests\ntest render::golden ... FAILED\nerror: test failed, to rerun pass `-p haider-tui --lib`\n",
        101,
    );
    push_tool(
        model,
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

/// The live proof's gesture: scrolled back into history under the origin
/// line, a click on the write's `(⌃O to expand)` door opens its detail.
#[test]
fn the_door_opens_the_detail_when_scrolled_back_under_the_origin_line() {
    let mut model = toolview_model();
    model.launch_origin = Some((1, Some("/private/tmp/origin/ws".into())));
    for n in 0..30 {
        push_agent(
            &mut model,
            &format!("tail-{n}"),
            &tuivirt_common::agent_row(n),
        );
    }
    draw(&model, 118, 36);
    model.scroll_back.set(model.scroll_max.get());
    let frame = draw(&model, 118, 36);
    let door = frame
        .row_containing("(⌃O to expand)")
        .unwrap_or_else(|| panic!("door on screen:\n{}", frame.rows.join("\n")));
    let (rect, hit) = frame
        .find_hit(|hit| matches!(hit, Hit::ToolDetail(_)))
        .expect("door hit");
    assert_eq!(usize::from(rect.y), door, "{}", frame.rows.join("\n"));
    model.handle_hit(hit);
    assert!(model.tool_detail.is_some());
    assert!(draw(&model, 118, 36).contains("full detail"));
}

// ---- 6. contrast, measured on the rendered cells ------------------------

fn rgb_of(
    color: ratatui::style::Color,
    fallback: haider_tui::theme::Rgb,
) -> haider_tui::theme::Rgb {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => haider_tui::theme::Rgb { r, g, b },
        _ => fallback,
    }
}

/// Every non-blank cell in `rows` must clear 4.5:1 against the ground it is
/// actually drawn on. Returns the offenders for one readable failure.
fn low_contrast_cells(
    frame: &tuivirt_common::Snapshot,
    rows: std::ops::Range<usize>,
    theme: &haider_tui::theme::Theme,
) -> Vec<String> {
    let mut bad = Vec::new();
    for y in rows {
        for (x, (symbol, style)) in frame.cells[y].iter().enumerate() {
            if symbol.trim().is_empty() {
                continue;
            }
            let fg = rgb_of(style.fg, theme.text);
            let bg = rgb_of(style.bg, theme.bg);
            let ratio = fg.contrast(bg);
            if ratio < 4.5 {
                bad.push(format!(
                    "{} ({x},{y}) {symbol:?} {ratio:.2} in {:?}",
                    theme.label, frame.rows[y]
                ));
            }
        }
    }
    bad
}

/// A transcript holding NOTHING but tool rows, covering every tone the
/// renderer can emit: ok/err/cancelled/running bullets, the `⎿` line in
/// each outcome, diff grounds, meaning-coloured output (verdicts, warning,
/// ERROR, `file:line` accents, ids, exit codes), fold heads, honesty
/// markers, bounded/show-all doors and a focused (hover-band) header.
fn every_tone_model() -> AppModel {
    let mut model = session_model();
    model.cwd = WORKSPACE.to_owned();
    model.clock_ms = 1_700_000_000_000;
    push_owner_tools(&mut model);
    let chatty: String = [
        "SHIP — candidate clears every gate",
        "HOLD pending evidence",
        "NO-SHIP: gate FAILED",
        "warning: 2 files skipped",
        "ERROR one match could not be read",
        "src/render.rs:5740:12 if model.todos_collapsed {",
        "thread_a1b2c3d4 run 9f3a2b7c1d finished ok",
        "exit 3 rc=0 status 0",
    ]
    .iter()
    .map(|line| format!("{line}\n"))
    .collect::<String>()
    .repeat(3);
    push_tool(
        &mut model,
        "grep",
        "grep",
        serde_json::json!({"query": "todos_collapsed", "glob": "*.rs"}),
        &chatty,
        ok(""),
    );
    push_tool(
        &mut model,
        "grep-all",
        "fs_search",
        serde_json::json!({"pattern": "x"}),
        &chatty,
        ok(""),
    );
    push_tool(
        &mut model,
        "recovered",
        "web_fetch",
        serde_json::json!({"url": "https://example.test"}),
        "",
        (
            ToolStatus::Completed,
            result(
                "{}",
                ToolResultStatus::Completed,
                Some("transient failure — retry 2/2 succeeded"),
            ),
        ),
    );
    push_tool(
        &mut model,
        "cancelled",
        "Monitor",
        serde_json::json!({"desc": "watch gate"}),
        "",
        (
            ToolStatus::Cancelled,
            result("", ToolResultStatus::Cancelled, Some("cancelled by user")),
        ),
    );
    for n in 0..2 {
        push_command(
            &mut model,
            &format!("ok-{n}"),
            "cargo fmt --check",
            "fmt ok\n",
            0,
        );
    }
    // A cut tail and an undecodable chunk wear their honesty markers.
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("big"),
            item: TurnItem::CommandExecution {
                call_id: "call-big".to_owned(),
                command: "yes | head -n 4000".to_owned(),
                status: ToolStatus::InProgress,
                exit_code: None,
            },
        }),
    );
    apply(
        &mut model,
        output_delta("big", &"y line of output\n".repeat(800)),
    );
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Delta {
            item_id: ItemId::new("big"),
            delta: ItemDelta::CommandOutput {
                stream: OutputStream::Stdout,
                chunk_b64: "*** not base64 ***".to_owned(),
            },
        }),
    );
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("live"),
            item: tool(
                "live",
                "Monitor",
                serde_json::json!({"desc": "still running"}),
                ToolStatus::InProgress,
            ),
        }),
    );
    model
        .edit_anchors
        .insert("edit-status".to_owned(), vec![Some(41)]);
    model.edit_anchor_revision += 1;
    model
}

/// Owner: the new rows keep ≥ 4.5:1 in every theme. Measured on RENDERED
/// cells — every glyph every tool-row state draws, against the ground it is
/// drawn on — so a tone nobody intended (Desert's `file:line` gold was
/// 4.01:1) cannot slip through again.
#[test]
fn every_rendered_tool_row_cell_clears_4_5_in_every_theme() {
    use haider_tui::toolfold::{RowState, Verbosity};
    let mut failures = Vec::new();
    for key in ThemeKey::ALL {
        let theme = key.theme();
        let check = |model: &AppModel, failures: &mut Vec<String>, what: &str| {
            let frame = draw(model, 120, 220);
            let area = frame.transcript;
            let rows = usize::from(area.y)..usize::from(area.y + area.height);
            for bad in low_contrast_cells(&frame, rows, theme) {
                failures.push(format!("{what}: {bad}"));
            }
        };
        let mut model = every_tone_model();
        model.theme = key;
        check(&model, &mut failures, "collapsed");
        model.toolfold.set_focus(Some("gate"));
        check(&model, &mut failures, "focused");
        model.toolfold.set_focus(None);
        model.toolfold.set("grep", RowState::Expanded);
        model.toolfold.set("grep-all", RowState::ShowAll);
        model.toolfold.set("edit-status", RowState::ShowAll);
        model.toolfold.set("big", RowState::Expanded);
        check(&model, &mut failures, "expanded");
        model.set_tool_verbosity(Verbosity::Verbose);
        check(&model, &mut failures, "verbose");
        model.set_tool_verbosity(Verbosity::Quiet);
        check(&model, &mut failures, "quiet");
        model.set_tool_verbosity(Verbosity::Normal);
        // The full-detail view of every tool row: the whole body is its.
        for id in model.tool_row_ids() {
            assert!(model.open_tool_detail(&id));
            let frame = draw(&model, 120, 220);
            let body = 0..usize::from(frame.height) - 1;
            for bad in low_contrast_cells(&frame, body, theme) {
                failures.push(format!("detail {id}: {bad}"));
            }
            model.close_tool_detail();
        }
    }
    assert!(
        failures.is_empty(),
        "{} low-contrast tool-row cells:\n{}",
        failures.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ---- 7. 973 repair 2: authoritative output, typed bounds, one click route

/// Astra item 1: a streamed tool whose 24 KiB stream overflowed the 8 KiB
/// tail but whose joined result is complete — the full-detail view shows the
/// result from its FIRST line; the collapsed row keeps the compact tail.
#[test]
fn full_detail_shows_the_complete_result_not_the_capped_stream() {
    let mut model = session_model();
    let stream = format!(
        "FIRST_STREAM_SENTINEL\n{}LAST_STREAM_SENTINEL\n",
        "stream line of output\n".repeat(1200)
    );
    assert!(stream.len() > 24 * 1024);
    push_tool(
        &mut model,
        "stream",
        "process_exec",
        serde_json::json!({"command": "produce a long log"}),
        &stream,
        ok(&stream),
    );
    let block = model
        .projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            haider_tui::projection::TranscriptEntry::Item(block)
                if block.item_id.as_str() == "stream" =>
            {
                Some(block)
            }
            _ => None,
        })
        .unwrap();
    assert!(
        block.output_truncated,
        "the stream really overflowed the tail"
    );
    assert!(
        !block
            .compact_output()
            .text
            .contains("FIRST_STREAM_SENTINEL")
    );
    let detail = block.detail_output();
    assert!(detail.text.starts_with("FIRST_STREAM_SENTINEL"));
    assert!(detail.stream_replaced && !detail.tail_cut);
    // Collapsed: the compact tail and its honest marker.
    let collapsed = draw(&model, 118, 36);
    assert!(collapsed.contains("bounded tail"));
    // Detail: the first line, reachable at the top; the last at the end.
    assert!(model.open_tool_detail("stream"));
    let top = draw(&model, 118, 36);
    assert!(
        top.contains("FIRST_STREAM_SENTINEL"),
        "{}",
        top.rows.join("\n")
    );
    assert!(top.contains("rows 1–"));
    model.scroll_tool_detail(isize::MAX);
    let end = draw(&model, 118, 36);
    assert!(
        end.contains("LAST_STREAM_SENTINEL"),
        "{}",
        end.rows.join("\n")
    );
    assert!(end.contains("shown above is the tool's complete result"));
    assert!(
        !end.contains("bounded tail"),
        "the shown text is not a tail"
    );
}

/// Astra item 3: a result the tool DECLARED bounded (typed `truncation`)
/// says so collapsed and in detail — counts and continuation, never the
/// digest.
#[test]
fn a_declared_bounded_result_says_so_in_every_view() {
    let mut model = session_model();
    let mut bounded = result("line one\nline two", ToolResultStatus::Completed, None);
    bounded.declare_truncation(haider_protocol::tool::ToolTruncation::from_bytes(
        &b"line one\nline two\nOMITTED THIRD LINE".repeat(200),
        bounded.preview.len(),
    ));
    bounded.cursor = Some("page-2".to_owned());
    let digest = bounded.truncation.as_ref().unwrap().sha256.clone();
    push_tool(
        &mut model,
        "cut",
        "fs_read",
        serde_json::json!({"path": "cut.txt"}),
        "",
        (ToolStatus::Completed, bounded),
    );
    let collapsed = draw(&model, 118, 36);
    assert!(
        collapsed.contains("result is bounded — 17 B of")
            && collapsed.contains("pageable (continuation cursor)"),
        "{}",
        collapsed.rows.join("\n")
    );
    assert!(
        collapsed.contains("⎿ Read 2 lines"),
        "the payload, without the marker line"
    );
    assert!(model.open_tool_detail("cut"));
    let detail = draw(&model, 118, 36);
    assert!(
        detail.contains("result is bounded"),
        "{}",
        detail.rows.join("\n")
    );
    for frame in [&collapsed, &detail] {
        assert!(
            !frame.rows.join("\n").contains(&digest[..16]),
            "no provenance digest"
        );
    }
    // An undeclared, complete result carries no such note.
    let mut plain = session_model();
    push_tool(
        &mut plain,
        "ok",
        "fs_read",
        serde_json::json!({"path": "a"}),
        "",
        ok("a\nb"),
    );
    assert!(!draw(&plain, 118, 36).contains("result is bounded"));
}

/// Astra item 4 / owner: expand by key AND click, Esc back to the same
/// place — for every row type. Each row's `⎿` line is a click route into
/// the full-detail view; a one-line write wider than the row gets a door.
#[test]
fn every_row_type_clicks_into_the_detail_view_and_esc_returns() {
    let mut model = session_model();
    model.cwd = WORKSPACE.to_owned();
    push_command(
        &mut model,
        "shell",
        "cargo test",
        "test result: ok\nmore\n",
        0,
    );
    push_tool(
        &mut model,
        "read",
        "fs_read",
        serde_json::json!({"path": "a.rs"}),
        "",
        ok("1\n2\n3"),
    );
    push_agent(&mut model, "sep-1", "between the reads and the writes");
    push_tool(
        &mut model,
        "write",
        "fs_write",
        serde_json::json!({"path": "b.md", "content": "one\ntwo\nthree\nfour\nfive\nsix\n"}),
        "",
        ok("wrote"),
    );
    push_tool(
        &mut model,
        "clip",
        "fs_write",
        serde_json::json!({"path": "wide.txt", "content": "W".repeat(400)}),
        "",
        ok("wrote"),
    );
    push_tool(
        &mut model,
        "edit",
        "fs_edit",
        serde_json::json!({"path": "c.rs", "edits": [{"old": "a", "new": "b"}]}),
        "",
        ok("edited"),
    );
    let before = draw(&model, 118, 36);
    assert!(
        before.contains("… line clipped (⌃O to expand)"),
        "{}",
        before.rows.join("\n")
    );
    for id in ["shell", "read", "write", "clip", "edit"] {
        let result_line = before
            .hits
            .iter()
            .filter(|(_, hit)| matches!(hit, Hit::ToolDetail(row) if row == id))
            .map(|(rect, _)| usize::from(rect.y))
            .collect::<Vec<_>>();
        assert!(
            result_line.iter().any(|y| before.rows[*y].contains('⎿')),
            "{id}: its ⎿ line is a click target: {:?}",
            result_line
        );
        let hit = before
            .hits
            .iter()
            .find(|(rect, hit)| {
                matches!(hit, Hit::ToolDetail(row) if row == id)
                    && before.rows[usize::from(rect.y)].contains('⎿')
            })
            .map(|(_, hit)| hit.clone())
            .unwrap();
        model.handle_hit(hit);
        let view = draw(&model, 118, 36);
        assert!(
            view.contains("full detail"),
            "{id}: {}",
            view.rows.join("\n")
        );
        model.handle(key(KeyCode::Esc));
        assert!(model.tool_detail.is_none());
        // Focus moved to the clicked row (the hover band); everything else
        // is exactly where it was.
        model.toolfold.set_focus(None);
        assert_eq!(draw(&model, 118, 36).rows, before.rows, "{id}: Esc returns");
    }
    // The clipped write's door and ⌃O show the WHOLE line.
    model.toolfold.set_focus(Some("clip"));
    model.handle(ctrl('o'));
    let view = draw(&model, 118, 36);
    let content = view
        .row_containing("Content · 1 line")
        .unwrap_or_else(|| panic!("{}", view.rows.join("\n")));
    let ws: usize = view.rows[content + 1..]
        .iter()
        .take_while(|row| !row.contains("Output") && !row.contains("Arguments"))
        .map(|row| row.matches('W').count())
        .sum();
    assert_eq!(
        ws, 400,
        "every character of the clipped line is in the view"
    );
    model.handle(key(KeyCode::Char('q')));
    assert!(model.tool_detail.is_none(), "q closes it too");
}

/// Astra item 2: an edit a LATER call may have overtaken is never
/// numbered from the moved file.
#[test]
fn an_edit_overtaken_by_a_later_call_is_not_numbered() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.md"), "a\nb\nNEW\nc\n").unwrap();
    let mut model = session_model();
    model.cwd = dir.path().display().to_string();
    model.clock_ms = 1_700_000_000_000;
    let args = serde_json::json!({"path": "f.md", "edits": [{"old": "old", "new": "NEW"}]});
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
            item: tool("e1", "fs_edit", args, ToolStatus::Completed),
        }),
    );
    // Before the next beat, a later command already ran.
    push_command(&mut model, "later", "sed -i '' 1d f.md", "", 0);
    model.clock_ms += 20;
    model.note_tool_timings();
    model.note_edit_anchors();
    assert_eq!(model.edit_anchors.get("e1"), Some(&vec![None]));
    let frame = draw(&model, 118, 36);
    assert!(
        frame.contains("- old") && frame.contains("+ NEW"),
        "{}",
        frame.rows.join("\n")
    );
    assert!(
        !frame.contains("3 + NEW"),
        "never numbered from the moved file"
    );
}

/// Astra item 5: the full-detail paint cost on a 1,000+ row transcript with
/// a 120,000-byte write. Prints timings; asserts only a generous ceiling so
/// the suite never flakes. `cargo test … -- --ignored --nocapture` for the
/// numbers recorded in the lane result.
#[test]
#[ignore = "timing measurement; run explicitly"]
fn detail_paint_cost_on_a_large_transcript() {
    use std::time::Instant;
    let mut model = session_model();
    for i in 0..1_200 {
        let (name, args, out) = match i % 4 {
            0 => (
                "fs_read",
                serde_json::json!({"path": format!("f{i}.rs")}),
                "one\ntwo\nthree",
            ),
            1 => (
                "process_exec",
                serde_json::json!({"command": "printf hello"}),
                "hello",
            ),
            2 => (
                "fs_edit",
                serde_json::json!({"path": format!("f{i}.rs"), "edits": [{"old": "one\ntwo\n", "new": "one\nchanged\n"}]}),
                "edited",
            ),
            _ => (
                "fs_write",
                serde_json::json!({"path": format!("f{i}.rs"), "content": "a\nb\nc\nd\ne\nf\n"}),
                "wrote",
            ),
        };
        push_tool(&mut model, &format!("t{i}"), name, args, "", ok(out));
    }
    let big: String = (1..=3000)
        .map(|n| format!("{n:05} {}end\n", "data ".repeat(6)))
        .collect();
    assert_eq!(big.len(), 120_000);
    push_tool(
        &mut model,
        "big",
        "fs_write",
        serde_json::json!({"path": "BIG.txt", "content": big}),
        "",
        ok("wrote 120000 bytes"),
    );
    let time = |model: &AppModel, width: u16, height: u16| {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut samples = Vec::new();
        for _ in 0..21 {
            let start = Instant::now();
            terminal
                .draw(|frame| {
                    haider_tui::render::render(model, frame);
                })
                .unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let first = samples.remove(0);
        samples.sort_by(f64::total_cmp);
        (first, samples[10], samples[19])
    };
    for (width, height) in [(118u16, 36u16), (80, 24)] {
        let collapsed = time(&model, width, height);
        assert!(model.open_tool_detail("big"));
        let detail = time(&model, width, height);
        model.scroll_tool_detail(isize::MAX);
        let detail_end = time(&model, width, height);
        // A taller paint at the end shows the diff's last line above the
        // closing sections (output, arguments).
        let frame = draw(&model, width, 60);
        assert!(
            frame.contains("03000"),
            "the last line is reachable:\n{}",
            frame.rows.join("\n")
        );
        model.close_tool_detail();
        println!(
            "TIMING {width}x{height} transcript=1201 tools: collapsed first={:.2}ms p50={:.2}ms p95={:.2}ms | \
             detail(120 KB write) first={:.2}ms p50={:.2}ms p95={:.2}ms | detail at end first={:.2}ms p50={:.2}ms p95={:.2}ms",
            collapsed.0,
            collapsed.1,
            collapsed.2,
            detail.0,
            detail.1,
            detail.2,
            detail_end.0,
            detail_end.1,
            detail_end.2
        );
        assert!(detail.1 < 250.0, "warm detail paint stays interactive");
    }
}

/// Live proof finding (repair 2): an execution tool's result is a JSON
/// ENVELOPE. The detail view shows the output it carries, with real
/// newlines — not one escaped line of digests — and says when the tool hit
/// its output limit.
#[test]
fn an_execution_envelope_shows_its_output_not_its_json() {
    let mut model = session_model();
    let output = format!(
        "FIRST_ENVELOPE_LINE\n{}LAST_ENVELOPE_LINE\n",
        "n\n".repeat(40)
    );
    let envelope = serde_json::json!({
        "artifact": "blake3:0123456789abcdef0123456789abcdef",
        "exit_code": 0,
        "limit_reached": null,
        "output": output,
        "status": "completed",
    })
    .to_string();
    push_tool(
        &mut model,
        "env",
        "process_exec",
        serde_json::json!({"command": "run"}),
        "",
        ok(&envelope),
    );
    let collapsed = draw(&model, 118, 36);
    assert!(
        collapsed.contains("⎿ FIRST_ENVELOPE_LINE … +41 lines"),
        "{}",
        collapsed.rows.join("\n")
    );
    assert!(model.open_tool_detail("env"));
    let view = draw(&model, 118, 60);
    let text = view.rows.join("\n");
    assert!(text.contains("      FIRST_ENVELOPE_LINE"), "{text}");
    assert!(text.contains("      LAST_ENVELOPE_LINE"), "{text}");
    assert!(!text.contains("blake3:"), "no digests: {text}");
    assert!(!text.contains("output limit"), "{text}");
    // An envelope whose tool hit its limit says so.
    let mut limited = session_model();
    let envelope =
        serde_json::json!({"limit_reached": "max_output_bytes", "output": "partial\n"}).to_string();
    push_tool(
        &mut limited,
        "lim",
        "process_exec",
        serde_json::json!({"command": "yes"}),
        "",
        ok(&envelope),
    );
    assert!(draw(&limited, 118, 36).contains("stopped at its output limit"));
    // Anything that is not such an envelope is shown exactly as sent.
    let mut raw = session_model();
    push_tool(
        &mut raw,
        "raw",
        "web_fetch",
        serde_json::json!({"url": "https://x.test"}),
        "",
        ok("{\"a\": 1}"),
    );
    assert!(raw.open_tool_detail("raw"));
    assert!(draw(&raw, 118, 36).contains("{\"a\": 1}"));
}
