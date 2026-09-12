//! tuivirt golden frames for the COLLAPSED tool rows (971-tui-collapse).
//!
//! The owner's spec pins the four row kinds — collapsed, expanded,
//! streaming, folded-`N` — at 80×24 and 120×40 in every theme. One scenario
//! carries all four kinds in a single transcript so the frames can be read
//! side by side with `state/reference/claude-code-collapsed-tool-rows.png`;
//! the mode/disclosure variants are pinned on the dark ground, where the
//! contrast complaint that started this wave was raised.
//!
//! These sizes are DELIBERATELY not `tuivirt_common::SIZES` (80×24, 118×36,
//! 160×50): the owner named 80×24 and 120×40, and adding a fourth size to
//! the shared list would regenerate all 100 pre-existing fixtures for no
//! visual reason.
//!
//! Regenerate ONLY deliberately: `UPDATE_TUIVIRT_GOLDENS=1 cargo test -p
//! haider-tui --test tuivirt_collapse_tests`, then review the fixture diff
//! line by line.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_tui::app::AppModel;
use haider_tui::theme::ThemeKey;
use haider_tui::toolfold::{RowState, ToolTiming, Verbosity};

mod tuivirt_common;
use tuivirt_common::{apply, check_golden, draw, push_agent, push_user, session_model};

/// The owner's two sizes for this wave.
const COLLAPSE_SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];

fn pin(name: &str, model: &AppModel) {
    for (width, height) in COLLAPSE_SIZES {
        check_golden(name, &draw(model, width, height));
    }
}

fn tool_completed(id: &str, name: &str, args: serde_json::Value) -> EventPayload {
    EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(id),
        item: TurnItem::ToolCall {
            call_id: format!("call-{id}"),
            name: name.to_owned(),
            args,
            status: ToolStatus::Completed,
        },
    })
}

fn tool_started(id: &str, name: &str, args: serde_json::Value) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new(id),
        item: TurnItem::ToolCall {
            call_id: format!("call-{id}"),
            name: name.to_owned(),
            args,
            status: ToolStatus::InProgress,
        },
    })
}

fn command_completed(id: &str, command: &str, exit_code: i32) -> EventPayload {
    EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(id),
        item: TurnItem::CommandExecution {
            call_id: format!("call-{id}"),
            command: command.to_owned(),
            status: if exit_code == 0 {
                ToolStatus::Completed
            } else {
                ToolStatus::Failed
            },
            exit_code: Some(exit_code),
        },
    })
}

/// One tool call driven the way a real stream drives it: `Started`, output
/// deltas, then `Completed`. Output only accumulates onto an OPEN block, so
/// a fixture that completes the item first measures nothing.
fn push_tool(model: &mut AppModel, id: &str, name: &str, args: serde_json::Value, body: &str) {
    apply(model, tool_started(id, name, args.clone()));
    if !body.is_empty() {
        apply(model, output(id, body));
    }
    apply(model, tool_completed(id, name, args));
}

fn push_command(model: &mut AppModel, id: &str, cmd: &str, body: &str) {
    apply(
        model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new(id),
            item: TurnItem::CommandExecution {
                call_id: format!("call-{id}"),
                command: cmd.to_owned(),
                status: ToolStatus::InProgress,
                exit_code: None,
            },
        }),
    );
    if !body.is_empty() {
        apply(model, output(id, body));
    }
    apply(model, command_completed(id, cmd, 0));
}

fn output(id: &str, text: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new(id),
        delta: ItemDelta::CommandOutput {
            stream: OutputStream::Stdout,
            chunk_b64: base64::engine::general_purpose::STANDARD.encode(text.as_bytes()),
        },
    })
}

/// A DETERMINISTIC observed duration, so a golden never rides a real clock.
fn timing(model: &mut AppModel, id: &str, ms: u64) {
    let started = model.clock_ms.saturating_sub(ms);
    model.tool_timings.insert(
        id.to_owned(),
        ToolTiming {
            started_ms: started,
            ended_ms: Some(model.clock_ms),
        },
    );
}

/// The wave's reference transcript: all four row kinds at once.
///
/// * `verify` — a settled tool call with output → COLLAPSED summary + `└`.
/// * `probe-1..3` — three consecutive shell commands → FOLDED (`Ran 3 …`).
/// * `grep` — a settled call the reader EXPANDED → bounded region.
/// * `monitor` — a live call → spinner + elapsed.
fn collapse_model() -> AppModel {
    let mut model = session_model();
    model.clock_ms = 1_700_000_000_000;
    push_user(&mut model, "run the gate and report");
    push_agent(
        &mut model,
        "reply-1",
        "Running the gate now — I will report the verdict.",
    );

    push_tool(
        &mut model,
        "verify",
        "verify",
        serde_json::json!({ "desc": "wave-971 candidate" }),
        "SHIP — wave-971 candidate clears every gate\nclippy: 0 warnings\ntest result: ok. 5166 passed; 0 failed\n",
    );
    timing(&mut model, "verify", 8_200);

    for (index, command) in [
        "cargo fmt --all --check",
        "cargo clippy -p haider-tui -- -D warnings",
        "cargo test -p haider-tui --locked",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("probe-{index}");
        push_command(
            &mut model,
            &id,
            command,
            &format!("Resuming agent a830387\n{command} finished\n"),
        );
        timing(&mut model, &id, 1_000 * (index as u64 + 1));
    }

    push_tool(
        &mut model,
        "grep",
        "grep",
        serde_json::json!({ "query": "todos_collapsed", "glob": "*.rs" }),
        "src/render.rs:5740:            if model.todos_collapsed {\nsrc/render.rs:6336: let arrow = …\nsrc/app.rs:5254:    pub todos_collapsed: bool,\nERROR one match could not be read\nwarning: 2 files skipped\nsrc/app.rs:5709: todos_collapsed: false,\nsrc/app.rs:16876: self.todos_collapsed = false;\nsrc/session.rs:127: pub todos_collapsed: bool,\nsrc/render.rs:6368: if !model.todos_collapsed {\nsrc/render.rs:6389: if !model.todos_collapsed {\nsrc/app.rs:17036: self.todos_collapsed = slot.todos_collapsed;\nsrc/app.rs:18069: self.todos_collapsed = !self.todos_collapsed;\n",
    );
    timing(&mut model, "grep", 400);
    model.toolfold.set("grep", RowState::Expanded);

    apply(
        &mut model,
        tool_started(
            "monitor",
            "Monitor",
            serde_json::json!({ "desc": "Astra 971-tui-collapse verification (re-armed)" }),
        ),
    );
    model.tool_timings.insert(
        "monitor".to_owned(),
        ToolTiming {
            started_ms: model.clock_ms.saturating_sub(3_000),
            ended_ms: None,
        },
    );
    model
}

// ---- 1. all four row kinds, every theme, both sizes --------------------

#[test]
fn collapsed_expanded_streaming_and_folded_rows_pin_on_every_theme() {
    for key in ThemeKey::ALL {
        let mut model = collapse_model();
        model.theme = key;
        pin(&format!("collapse_tools_{}", key.name()), &model);
    }
}

// ---- 2. the disclosure and mode variants (dark ground) -----------------

#[test]
fn quiet_mode_drops_the_subline_frames() {
    let mut model = collapse_model();
    model.theme = ThemeKey::Dark;
    model.set_tool_verbosity(Verbosity::Quiet);
    pin("collapse_tools_quiet", &model);
}

#[test]
fn verbose_mode_opens_every_row_and_folds_nothing_frames() {
    let mut model = collapse_model();
    model.theme = ThemeKey::Dark;
    model.set_tool_verbosity(Verbosity::Verbose);
    pin("collapse_tools_verbose", &model);
}

#[test]
fn show_all_lifts_the_bounded_region_frames() {
    let mut model = collapse_model();
    model.theme = ThemeKey::Dark;
    model.toolfold.set("grep", RowState::ShowAll);
    pin("collapse_tools_show_all", &model);
}

#[test]
fn an_unfolded_run_renders_its_members_frames() {
    let mut model = collapse_model();
    model.theme = ThemeKey::Dark;
    model.toolfold.toggle_fold("probe-0");
    pin("collapse_tools_unfolded", &model);
}

#[test]
fn the_focused_row_wears_the_hover_band_frames() {
    let mut model = collapse_model();
    model.theme = ThemeKey::Dark;
    model.toolfold.set_focus(Some("verify"));
    pin("collapse_tools_focused", &model);
}

#[test]
fn expand_all_frames() {
    let mut model = collapse_model();
    model.theme = ThemeKey::Dark;
    model.toggle_all_tool_rows();
    pin("collapse_tools_expand_all", &model);
}

// ---- 3. a transcript with no tool rows at all --------------------------

#[test]
fn a_transcript_without_tool_rows_is_untouched_frames() {
    let mut model = session_model();
    model.theme = ThemeKey::Dark;
    push_user(&mut model, "just a question");
    push_agent(&mut model, "reply-1", "Just an answer — no tools were run.");
    pin("collapse_tools_absent", &model);
}

// ---- 4. the band's live background-task line (owner addition) ---------

fn shell_row(id: &str, title: &str) -> haider_rpc::ShellWire {
    haider_rpc::ShellWire {
        id: id.to_owned(),
        kind: haider_rpc::ShellKindWire::Local,
        status: haider_rpc::ShellStatusWire::Running,
        title: title.to_owned(),
        cwd_or_host: "/workspace/haider".to_owned(),
        created_at_ms: 1_699_999_880_000,
        last_activity_ms: 1_699_999_990_000,
        bytes_out: 4_096,
    }
}

fn monitor_row(id: &str, summary: &str, last: Option<&str>) -> haider_rpc::MonitorRegistrationWire {
    haider_rpc::MonitorRegistrationWire {
        monitor_id: id.to_owned(),
        session_id: haider_protocol::ids::SessionId::new("collapse-session"),
        branch_id: None,
        agent_id: None,
        source: haider_rpc::MonitorSourceWire::Timer {
            interval_ms: 60_000,
        },
        filter: None,
        action: haider_rpc::MonitorActionWire {
            report: true,
            follow_up: None,
        },
        occurrence: haider_rpc::MonitorOccurrenceWire::Every,
        created_at_ms: 1_699_999_400_000,
        start_source_sequence: 0,
        expires_at_ms: None,
        state: haider_rpc::MonitorStateWire::Armed,
        last_event: last.map(|summary| haider_rpc::MonitorLastEventWire {
            at_ms: 1_699_999_900_000,
            summary: summary.to_owned(),
        }),
        fire_count: 2,
        next_fire_at_ms: Some(1_700_000_060_000),
        source_summary: summary.to_owned(),
    }
}

/// A session whose daemon reported the auto-allow posture and is running
/// `shells` shells and `monitors` monitors.
fn tasks_model(shells: usize, monitors: usize) -> AppModel {
    let mut model = session_model();
    model.clock_ms = 1_700_000_000_000;
    push_user(&mut model, "keep the wave moving");
    model.session_permissions.insert(
        model
            .active_session
            .clone()
            .expect("the fixture attaches a session"),
        haider_protocol::session::SessionPermissionOverridesV1 {
            read_only: false,
            allow_writes: true,
            allow_exec: true,
            allow_mobile: false,
            auto_allow: true,
        },
    );
    model.shells = (0..shells)
        .map(|n| shell_row(&format!("sh-{n}"), &format!("gate {n}")))
        .collect();
    model.monitors = (0..monitors)
        .map(|n| {
            monitor_row(
                &format!("mon-{n}"),
                &format!("poll gate {n} · every 60s"),
                (n % 2 == 0).then_some("Checking verify-10-resume exit-code gate"),
            )
        })
        .collect();
    model.monitor_count = model.monitors.len();
    model
}

#[test]
fn the_task_line_pins_at_zero_tasks_on_every_theme() {
    for key in ThemeKey::ALL {
        let mut model = tasks_model(0, 0);
        model.theme = key;
        pin(&format!("task_line_none_{}", key.name()), &model);
    }
}

#[test]
fn the_task_line_pins_with_a_few_tasks_on_every_theme() {
    for key in ThemeKey::ALL {
        let mut model = tasks_model(1, 2);
        model.theme = key;
        pin(&format!("task_line_few_{}", key.name()), &model);
    }
}

#[test]
fn the_task_line_pins_with_many_tasks_on_every_theme() {
    for key in ThemeKey::ALL {
        let mut model = tasks_model(6, 14);
        model.theme = key;
        pin(&format!("task_line_many_{}", key.name()), &model);
    }
}

#[test]
fn the_expanded_task_list_pins_at_both_sizes() {
    let mut model = tasks_model(6, 14);
    model.theme = ThemeKey::Dark;
    model.toggle_tasks_line();
    pin("task_line_expanded", &model);
}

#[test]
fn overlays_clear_long_transcript_cells() {
    let mut model = session_model();
    for i in 0..60 {
        push_agent(
            &mut model,
            &format!("overlay-{i}"),
            &"TRANSCRIPT_SENTINEL ".repeat(12),
        );
    }
    assert!(
        draw(&model, 120, 40)
            .rows
            .iter()
            .any(|row| row.contains("TRANSCRIPT_SENTINEL"))
    );
    model.help_open = true;
    for (width, height) in COLLAPSE_SIZES {
        let snapshot = draw(&model, width, height);
        assert!(
            !snapshot
                .rows
                .iter()
                .any(|row| row.contains("TRANSCRIPT_SENTINEL"))
        );
        check_golden("help_over_long_transcript", &snapshot);
    }
    model.help_open = false;
    model.shells_open = true;
    for (width, height) in COLLAPSE_SIZES {
        let snapshot = draw(&model, width, height);
        let row = snapshot
            .rows
            .iter()
            .position(|row| row.starts_with("shells  "))
            .expect("shell panel");
        assert_eq!(snapshot.rows[row + 1].trim(), "no terminal sessions");
        assert!(snapshot.rows[row + 2].trim().is_empty());
        check_golden("empty_shells_over_long_transcript", &snapshot);
    }
}
