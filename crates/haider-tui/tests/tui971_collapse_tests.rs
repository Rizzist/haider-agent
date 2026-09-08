//! 971-tui-collapse pins — collapsed tool rows, the fold, the disclosure
//! gestures, per-session persistence, and colour BY MEANING.
//!
//! Owner (2026-09-08): "all this darker text in the TUI is making navigation
//! difficult … tool responses should be compressed by default similar to
//! Claude Code, then expand if needed by the user (can go back to compressed
//! from the main TUI)."
#![allow(clippy::expect_used, clippy::unwrap_used)]

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::ids::{ItemId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_tui::app::{AppEvent, AppModel, Hit, RuntimeMode, Screen};
use haider_tui::toolfold::{
    self as tf, RowFacts, RowState, Segment, Tone, ToolFold, ToolTiming, Verbosity,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// ---------------------------------------------------------- fixtures ----

fn session_id() -> SessionId {
    SessionId::new("collapse-session")
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    model.identity.device = "test-lion-box".to_owned();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&session_id());
    model.open_session(&session_id());
    model.requests.clear();
    model.screen = Screen::Session;
    model.clock_ms = 1_700_000_000_000;
    model
}

/// Type a slash line and send it — the public key path, never a private
/// reducer entry, because the fallback's whole point is that it survives
/// the keyboard.
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

fn apply(model: &mut AppModel, payload: EventPayload) {
    model.projection.apply(&payload);
}

fn output(model: &mut AppModel, id: &str, text: &str) {
    apply(
        model,
        EventPayload::Item(ItemEvent::Delta {
            item_id: ItemId::new(id),
            delta: ItemDelta::CommandOutput {
                stream: OutputStream::Stdout,
                chunk_b64: base64::engine::general_purpose::STANDARD.encode(text.as_bytes()),
            },
        }),
    );
}

/// One tool call, driven the way a real stream drives it: `Started`, then
/// any output deltas, then `Completed`. Output only accumulates onto an OPEN
/// block (`SessionProjection::open_block_mut`), so a fixture that lands a
/// completed item first and then pushes output is measuring nothing.
fn tool(model: &mut AppModel, id: &str, name: &str, status: ToolStatus, desc: &str) {
    tool_out(model, id, name, status, desc, "");
}

fn tool_out(
    model: &mut AppModel,
    id: &str,
    name: &str,
    status: ToolStatus,
    desc: &str,
    body: &str,
) {
    let item = |status| TurnItem::ToolCall {
        call_id: format!("call-{id}"),
        name: name.to_owned(),
        args: serde_json::json!({ "desc": desc }),
        status,
    };
    apply(
        model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new(id),
            item: item(ToolStatus::InProgress),
        }),
    );
    if !body.is_empty() {
        output(model, id, body);
    }
    if !matches!(status, ToolStatus::Pending | ToolStatus::InProgress) {
        apply(
            model,
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(id),
                item: item(status),
            }),
        );
    }
}

/// Land a terminal status on an already-open call, without re-`Started`ing it.
fn settle(model: &mut AppModel, id: &str, name: &str, status: ToolStatus, desc: &str) {
    apply(
        model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item: TurnItem::ToolCall {
                call_id: format!("call-{id}"),
                name: name.to_owned(),
                args: serde_json::json!({ "desc": desc }),
                status,
            },
        }),
    );
}

fn command(model: &mut AppModel, id: &str, cmd: &str, exit_code: i32) {
    command_out(model, id, cmd, exit_code, "");
}

fn command_out(model: &mut AppModel, id: &str, cmd: &str, exit_code: i32, body: &str) {
    let item = |status, exit| TurnItem::CommandExecution {
        call_id: format!("call-{id}"),
        command: cmd.to_owned(),
        status,
        exit_code: exit,
    };
    apply(
        model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new(id),
            item: item(ToolStatus::InProgress, None),
        }),
    );
    if !body.is_empty() {
        output(model, id, body);
    }
    apply(
        model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item: item(
                if exit_code == 0 {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Failed
                },
                Some(exit_code),
            ),
        }),
    );
}

/// The rendered rows of one frame — what the reader actually sees.
fn rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("test backend must build");
    terminal
        .draw(|frame| {
            haider_tui::render::render(model, frame);
        })
        .expect("render must succeed");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn transcript_has(model: &AppModel, needle: &str) -> bool {
    rows(model, 120, 40).iter().any(|row| row.contains(needle))
}

fn facts(name: &str, args: &str) -> RowFacts<'static> {
    RowFacts {
        name: name.to_owned(),
        name_tone: Tone::Name,
        args: args.to_owned(),
        glyph: "✓",
        glyph_tone: Tone::Ok,
        ..RowFacts::default()
    }
}

// ---- 1. the row FORMAT -------------------------------------------------

#[test]
fn a_collapsed_summary_row_carries_name_args_exit_lines_and_duration() {
    let facts = RowFacts {
        exit_code: Some(0),
        output_lines: 412,
        elapsed_ms: Some(8_200),
        ..facts("bash", "cargo test -p haider-tui")
    };
    let text = tf::segments_text(&tf::summary_segments(&facts, 0, 0));
    assert_eq!(
        text, "  ✓ bash(cargo test -p haider-tui) · exit 0 · 412 lines · 8s",
        "the summary row is name + argument summary · exit code · line count · duration"
    );
}

#[test]
fn a_nonzero_exit_code_wears_the_error_ink_and_a_zero_one_does_not() {
    let toned = |code: i32| {
        let facts = RowFacts {
            exit_code: Some(code),
            ..facts("bash", "false")
        };
        tf::summary_segments(&facts, 0, 0)
            .into_iter()
            .find(|segment| segment.text.contains("exit"))
            .expect("the exit segment must exist")
            .tone
    };
    assert_eq!(toned(0), Tone::Meta, "exit 0 is metadata, not an alarm");
    assert_eq!(toned(1), Tone::Err, "a non-zero exit code is coloured");
    assert_eq!(toned(-1), Tone::Err);
}

#[test]
fn one_retained_line_is_singular_and_zero_lines_drops_the_segment() {
    let with = |lines: usize| {
        tf::segments_text(&tf::summary_segments(
            &RowFacts {
                output_lines: lines,
                ..facts("grep", "todo")
            },
            0,
            0,
        ))
    };
    assert!(with(1).contains("· 1 line"), "{}", with(1));
    assert!(!with(1).contains("1 lines"));
    assert!(
        !with(0).contains("line"),
        "no output means NO segment, never `0 lines`: {}",
        with(0)
    );
}

#[test]
fn a_row_whose_start_was_never_observed_omits_its_duration_rather_than_faking_zero() {
    let text = tf::segments_text(&tf::summary_segments(&facts("fs_read", "src/app.rs"), 0, 0));
    assert!(
        !text.contains("0s"),
        "an unobserved duration is absent, never a fabricated 0s: {text}"
    );
}

#[test]
fn a_streaming_row_shows_a_spinner_and_its_elapsed_time() {
    let live = RowFacts {
        streaming: true,
        spinner: true,
        elapsed_ms: Some(3_000),
        glyph_tone: Tone::Warn,
        ..facts("Monitor", "Astra 971 verification")
    };
    let first = tf::segments_text(&tf::summary_segments(&live, 0, 0));
    let later = tf::segments_text(&tf::summary_segments(&live, 1, 0));
    assert!(first.contains("· running… 3s"), "{first}");
    assert_ne!(
        first, later,
        "the spinner must ADVANCE on the shared animation clock"
    );
    assert_eq!(tf::spinner_frame(0), tf::SPINNER[0]);
    // A shell command's `$`/`!` sigil is PROVENANCE, so the spinner never
    // takes it: a running user `!` command must still look like one.
    let live_command = RowFacts {
        streaming: true,
        spinner: false,
        glyph: "!",
        ..facts("!", "")
    };
    let text = tf::segments_text(&tf::summary_segments(&live_command, 1, 0));
    assert!(text.starts_with("  ! "), "{text}");
    assert_eq!(
        tf::spinner_frame(4),
        tf::SPINNER[0],
        "the spinner wraps its frame set"
    );
}

#[test]
fn the_argument_summary_is_the_only_segment_a_narrow_row_sacrifices() {
    let facts = RowFacts {
        exit_code: Some(2),
        output_lines: 9,
        ..facts(
            "bash",
            "cargo test --workspace --locked --all-features -- --nocapture",
        )
    };
    let narrow = tf::segments_text(&tf::summary_segments(&facts, 0, 56));
    assert!(narrow.chars().count() <= 56, "{narrow}");
    assert!(
        narrow.contains("exit 2") && narrow.contains("9 lines"),
        "the figures a reader scans for survive; the arguments ellipsize: {narrow}"
    );
    assert!(narrow.contains('…'), "{narrow}");
}

#[test]
fn the_subline_is_the_first_meaningful_result_line() {
    let segments = tf::subline_segments(
        "\n   \n────────────\nSHIP — the candidate clears every gate\nsecond line\n",
        0,
    )
    .expect("a meaningful line exists");
    let text = tf::segments_text(&segments);
    assert!(text.starts_with("    └ "), "{text}");
    assert!(
        text.contains("SHIP — the candidate clears every gate"),
        "{text}"
    );
    assert!(!text.contains("second line"), "{text}");
    assert!(
        tf::subline_segments("\n\n───\n", 0).is_none(),
        "a rule-only result grows no elbow"
    );
}

// ---- 2. colour BY MEANING ---------------------------------------------

fn tone_of(line: &str, needle: &str) -> Tone {
    tf::meaning_segments(line)
        .into_iter()
        .find(|segment| segment.text.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` must appear in `{line}`"))
        .tone
}

#[test]
fn verdicts_are_coloured_by_meaning_not_by_source() {
    assert_eq!(tone_of("SHIP — candidate clears", "SHIP"), Tone::Ok);
    assert_eq!(tone_of("HOLD pending evidence", "HOLD"), Tone::Warn);
    assert_eq!(tone_of("NO-SHIP: gate failed", "NO-SHIP"), Tone::Err);
    assert_eq!(tone_of("FAIL after 2 attempts", "FAIL"), Tone::Err);
    assert_eq!(tone_of("ERROR reading manifest", "ERROR"), Tone::Err);
    assert_eq!(tone_of("error: unresolved import", "error:"), Tone::Err);
    assert_eq!(tone_of("warning: unused import", "warning:"), Tone::Warn);
}

#[test]
fn lowercase_prose_is_not_a_verdict() {
    assert_eq!(
        tone_of("we should ship this tomorrow", "ship"),
        Tone::Meta,
        "`SHIP` is a verdict; `ship` is English"
    );
    assert_eq!(tone_of("hold the door", "hold"), Tone::Meta);
}

#[test]
fn exit_codes_inside_output_are_coloured_by_their_value() {
    assert_eq!(tone_of("process exited 0 cleanly", "0"), Tone::Meta);
    assert_eq!(tone_of("process exited 137 killed", "137"), Tone::Err);
    assert_eq!(tone_of("rc=0", "rc=0"), Tone::Meta);
    assert_eq!(tone_of("rc=1", "rc=1"), Tone::Err);
}

#[test]
fn file_line_coordinates_and_ids_keep_their_own_ink() {
    assert_eq!(
        tone_of("at src/app.rs:5182 in", "src/app.rs:5182"),
        Tone::Accent
    );
    assert_eq!(
        tone_of("render.rs:6117:9 warn", "render.rs:6117:9"),
        Tone::Accent
    );
    assert_eq!(
        tone_of("Resuming agent a830387", "a830387"),
        Tone::Name,
        "a commit-ish id is identity ink"
    );
    assert_eq!(tone_of("task b2fqr12u4 armed", "task"), Tone::Meta);
    assert_eq!(
        tone_of("thread thr_9fa21b7 open", "thr_9fa21b7"),
        Tone::Name
    );
    assert_eq!(
        tone_of("the quick brown fox", "quick"),
        Tone::Meta,
        "ordinary words are metadata ink, which now clears 4.5:1"
    );
}

#[test]
fn meaning_segments_reproduce_the_line_verbatim() {
    for line in [
        "SHIP · exit 0 · src/app.rs:12 · a830387",
        "   leading and trailing   ",
        "",
        "one",
    ] {
        assert_eq!(
            tf::segments_text(&tf::meaning_segments(line)),
            line,
            "the tokenizer may only re-INK a line, never rewrite it"
        );
    }
}

// ---- 3. default collapsed, and the toggles ----------------------------

#[test]
fn tool_rows_are_collapsed_by_default() {
    let mut model = session_model();
    tool_out(
        &mut model,
        "t1",
        "verify",
        ToolStatus::Completed,
        "candidate",
        "SHIP — clears every gate\nline two\nline three\n",
    );
    assert_eq!(model.toolfold.state_of("t1"), RowState::Collapsed);
    assert!(transcript_has(&model, "SHIP — clears every gate"));
    assert!(
        !transcript_has(&model, "line three"),
        "only the FIRST meaningful result line survives a collapsed row"
    );
}

#[test]
fn one_row_toggles_collapsed_expanded_show_all_and_back() {
    let mut fold = ToolFold::default();
    assert_eq!(fold.state_of("t1"), RowState::Collapsed);
    fold.cycle("t1");
    assert_eq!(fold.state_of("t1"), RowState::Expanded);
    fold.cycle("t1");
    assert_eq!(fold.state_of("t1"), RowState::ShowAll);
    fold.cycle("t1");
    assert_eq!(
        fold.state_of("t1"),
        RowState::Collapsed,
        "the reader gets back to the compressed view with the gesture that opened it"
    );
}

#[test]
fn a_click_toggles_exactly_the_row_it_was_painted_for() {
    let mut model = session_model();
    tool_out(
        &mut model,
        "t1",
        "verify",
        ToolStatus::Completed,
        "one",
        "first\nsecond\nthird\n",
    );
    tool_out(
        &mut model,
        "t2",
        "grep",
        ToolStatus::Completed,
        "two",
        "alpha\nbeta\n",
    );
    model.handle_hit(Hit::ToolRowToggle("t2".to_owned()));
    assert_eq!(model.toolfold.state_of("t2"), RowState::Expanded);
    assert_eq!(
        model.toolfold.state_of("t1"),
        RowState::Collapsed,
        "a value-carrying hit can only ever act on its own row"
    );
    assert_eq!(model.toolfold.focus(), Some("t2"), "a click also focuses");
}

#[test]
fn toggle_all_expands_every_row_and_drops_per_row_overrides() {
    let mut model = session_model();
    tool(&mut model, "t1", "verify", ToolStatus::Completed, "one");
    model.toolfold.set("t1", RowState::ShowAll);
    model.toggle_all_tool_rows();
    assert!(model.toolfold.all_expanded());
    assert_eq!(
        model.toolfold.state_of("t1"),
        RowState::Expanded,
        "⌥T is all-or-nothing: a leftover override would refuse the gesture"
    );
    model.toggle_all_tool_rows();
    assert!(!model.toolfold.all_expanded());
    assert_eq!(model.toolfold.state_of("t1"), RowState::Collapsed);
}

#[test]
fn the_focus_walks_the_tool_rows_and_releases_off_the_top() {
    let mut model = session_model();
    tool(&mut model, "t1", "verify", ToolStatus::Completed, "one");
    tool(&mut model, "t2", "grep", ToolStatus::Completed, "two");
    assert_eq!(
        model.toolfold.focus(),
        None,
        "the resting state is unfocused"
    );
    model.move_tool_focus(true);
    assert_eq!(
        model.toolfold.focus(),
        Some("t2"),
        "the first move lands on the NEWEST row"
    );
    model.move_tool_focus(false);
    assert_eq!(model.toolfold.focus(), Some("t1"));
    model.move_tool_focus(false);
    assert_eq!(
        model.toolfold.focus(),
        None,
        "backwards off the first row hands ⏎/Space back to the composer"
    );
}

#[test]
fn enter_cycles_the_focused_row_only_while_nothing_is_drafted() {
    let mut model = session_model();
    tool(&mut model, "t1", "verify", ToolStatus::Completed, "one");
    assert!(
        !model.cycle_focused_tool_row(),
        "with no focus the composer keeps ⏎"
    );
    model.toolfold.set_focus(Some("t1"));
    assert!(model.cycle_focused_tool_row());
    assert_eq!(model.toolfold.state_of("t1"), RowState::Expanded);
}

// ---- 4. the fold ------------------------------------------------------

#[test]
fn consecutive_same_tool_calls_fold_into_one_row_with_a_count() {
    let mut model = session_model();
    for index in 0..3 {
        let id = format!("p{index}");
        command_out(
            &mut model,
            &id,
            &format!("echo {index}"),
            0,
            "Resuming agent a830387\n",
        );
    }
    let runs = haider_tui::render::foldable_runs(&model.projection, &model.toolfold);
    assert_eq!(runs.len(), 1, "one maximal run");
    assert_eq!(runs[0].len, 3);
    assert!(transcript_has(&model, "Ran 3 shell commands"));
    assert!(
        transcript_has(&model, "Resuming agent a830387"),
        "the fold still speaks with the LATEST member's result line"
    );
    assert!(
        !transcript_has(&model, "echo 1"),
        "the members' own rows are hidden while the run is folded"
    );
}

#[test]
fn a_run_of_one_never_folds() {
    let mut model = session_model();
    command(&mut model, "p0", "echo one", 0);
    assert!(haider_tui::render::foldable_runs(&model.projection, &model.toolfold).is_empty());
    assert!(transcript_has(&model, "echo one"));
}

#[test]
fn a_streaming_call_breaks_the_run() {
    let mut model = session_model();
    command(&mut model, "p0", "echo one", 0);
    tool(
        &mut model,
        "live",
        "bash",
        ToolStatus::InProgress,
        "echo two",
    );
    command(&mut model, "p1", "echo three", 0);
    assert!(
        haider_tui::render::foldable_runs(&model.projection, &model.toolfold).is_empty(),
        "a live row is exactly what the reader is watching — it never hides"
    );
}

#[test]
fn opening_a_fold_head_renders_its_members_again() {
    let mut model = session_model();
    for index in 0..3 {
        command(
            &mut model,
            &format!("p{index}"),
            &format!("echo {index}"),
            0,
        );
    }
    model.handle_hit(Hit::ToolFoldToggle("p0".to_owned()));
    assert!(model.toolfold.is_unfolded("p0"));
    assert!(haider_tui::render::foldable_runs(&model.projection, &model.toolfold).is_empty());
    assert!(transcript_has(&model, "echo 1"));
    assert!(!transcript_has(&model, "Ran 3 shell commands"));
}

#[test]
fn verbose_mode_folds_nothing() {
    let mut model = session_model();
    for index in 0..3 {
        command(
            &mut model,
            &format!("p{index}"),
            &format!("echo {index}"),
            0,
        );
    }
    model.set_tool_verbosity(Verbosity::Verbose);
    assert!(
        haider_tui::render::foldable_runs(&model.projection, &model.toolfold).is_empty(),
        "a reader who asked to see everything is not shown a summary instead"
    );
}

#[test]
fn a_shell_run_folds_under_the_references_own_noun() {
    assert_eq!(tf::fold_noun("command", 3), "shell commands");
    assert_eq!(tf::fold_noun("bash", 1), "shell command");
    assert_eq!(tf::fold_noun("web_fetch", 4), "web_fetch calls");
    let run = tf::FoldRun {
        start: 0,
        len: 2,
        name: "command".to_owned(),
        subline: None,
        truncated: false,
        decode_error: false,
    };
    assert_eq!(
        tf::segments_text(&tf::fold_segments(&run)),
        "  Ran 2 shell commands"
    );
    // A fold row never swallows an honesty marker.
    let bounded = tf::FoldRun {
        truncated: true,
        decode_error: true,
        ..run
    };
    assert_eq!(
        tf::segments_text(&tf::fold_segments(&bounded)),
        "  Ran 2 shell commands · bounded tails · ⚠ undecodable output"
    );
}

// ---- 5. the bounded expanded region -----------------------------------

#[test]
fn an_expanded_row_is_bounded_and_offers_show_all() {
    let mut model = session_model();
    let body: String = (0..40).map(|n| format!("hit number {n}\n")).collect();
    tool_out(
        &mut model,
        "t1",
        "grep",
        ToolStatus::Completed,
        "many hits",
        &body,
    );
    model.toolfold.set("t1", RowState::Expanded);
    let hidden = 40 - tf::EXPANDED_MAX_ROWS;
    assert!(transcript_has(&model, "hit number 0"));
    assert!(
        !transcript_has(&model, &format!("hit number {}", tf::EXPANDED_MAX_ROWS)),
        "the expanded region stops at EXPANDED_MAX_ROWS"
    );
    assert!(transcript_has(&model, &format!("⋯ {hidden} more lines")));
    model.handle_hit(Hit::ToolShowAll("t1".to_owned()));
    assert_eq!(model.toolfold.state_of("t1"), RowState::ShowAll);
    assert!(transcript_has(&model, "hit number 39"));
}

#[test]
fn show_all_is_singular_for_one_hidden_line() {
    assert_eq!(
        tf::segments_text(&tf::show_all_segments(1)),
        "    └ ⋯ 1 more line · ⏎ show all"
    );
}

// ---- 6. verbosity -----------------------------------------------------

#[test]
fn the_verbosity_cycle_is_quiet_normal_verbose() {
    let mut model = session_model();
    assert_eq!(
        model.toolfold.verbosity(),
        Verbosity::Normal,
        "normal is the owner's default"
    );
    model.cycle_tool_verbosity();
    assert_eq!(model.toolfold.verbosity(), Verbosity::Verbose);
    model.cycle_tool_verbosity();
    assert_eq!(model.toolfold.verbosity(), Verbosity::Quiet);
    model.cycle_tool_verbosity();
    assert_eq!(model.toolfold.verbosity(), Verbosity::Normal);
    assert_eq!(
        model.verbosity_commits, 3,
        "every commit bumps the persistence counter"
    );
}

#[test]
fn quiet_drops_the_subline_and_verbose_opens_the_row() {
    let mut model = session_model();
    tool_out(
        &mut model,
        "t1",
        "verify",
        ToolStatus::Completed,
        "candidate",
        "SHIP — clears every gate\nsecond line\n",
    );
    model.set_tool_verbosity(Verbosity::Quiet);
    assert!(transcript_has(&model, "verify(candidate)"));
    assert!(
        !transcript_has(&model, "SHIP — clears every gate"),
        "quiet is the summary row alone"
    );
    model.set_tool_verbosity(Verbosity::Verbose);
    assert_eq!(model.toolfold.state_of("t1"), RowState::Expanded);
    assert!(transcript_has(&model, "second line"));
}

#[test]
fn a_verbosity_name_round_trips_and_an_unknown_one_is_refused() {
    for verbosity in Verbosity::ALL {
        assert_eq!(Verbosity::parse(verbosity.name()), Some(verbosity));
    }
    assert_eq!(Verbosity::parse("loud"), None);
    for state in [RowState::Collapsed, RowState::Expanded, RowState::ShowAll] {
        assert_eq!(RowState::parse(state.name()), Some(state));
    }
    assert_eq!(RowState::parse("half"), None);
}

// ---- 7. persistence ---------------------------------------------------

#[test]
fn the_expanded_rows_survive_leaving_and_re_entering_the_session() {
    let mut model = session_model();
    let other = SessionId::new("collapse-other");
    model.upsert_live_session(&other);
    tool(
        &mut model,
        "t1",
        "verify",
        ToolStatus::Completed,
        "candidate",
    );
    model.toolfold.set("t1", RowState::ShowAll);
    model.toggle_all_tool_rows();
    let all_expanded = model.toolfold.all_expanded();

    model.open_session(&other);
    assert_eq!(
        model.toolfold.state_of("t1"),
        RowState::Collapsed,
        "another session opens on the collapsed default, not the first one's state"
    );

    model.open_session(&session_id());
    assert_eq!(model.toolfold.all_expanded(), all_expanded);
    assert_eq!(
        model.toolfold.state_of("t1"),
        RowState::Expanded,
        "re-entering restores exactly what the reader left (⌥T dropped the override)"
    );
}

#[test]
fn the_verbosity_persists_to_the_profile_and_reloads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tui-settings.json");
    let mut store = haider_tui::settings::SettingsStore::at(path.clone());
    store.save_verbosity_if_changed(haider_tui::theme::ThemeChoice::System, Verbosity::Quiet);
    let reopened = haider_tui::settings::SettingsStore::at(path.clone());
    assert_eq!(reopened.load_verbosity(), Verbosity::Quiet);
    assert_eq!(
        reopened.load(),
        Some(haider_tui::theme::ThemeChoice::System),
        "a verbosity write carries the theme rather than dropping it"
    );

    // A theme save must not drop the verbosity it inherited.
    let mut reopened = haider_tui::settings::SettingsStore::at(path.clone());
    reopened.set_verbosity(Verbosity::Quiet);
    reopened.save_if_changed(haider_tui::theme::ThemeChoice::Fixed(
        haider_tui::theme::ThemeKey::Desert,
    ));
    let third = haider_tui::settings::SettingsStore::at(path);
    assert_eq!(third.load_verbosity(), Verbosity::Quiet);
}

#[test]
fn a_pre_wave_settings_file_loads_as_the_normal_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tui-settings.json");
    std::fs::write(
        &path,
        br#"{"version":1,"theme":"dark","notifications":true}"#,
    )
    .expect("seed the pre-wave file");
    let store = haider_tui::settings::SettingsStore::at(path);
    assert_eq!(
        store.load_verbosity(),
        Verbosity::Normal,
        "an additive field's absence is the default, never a refused load"
    );
    assert_eq!(
        store.load(),
        Some(haider_tui::theme::ThemeChoice::Fixed(
            haider_tui::theme::ThemeKey::Dark
        ))
    );
}

#[test]
fn detaching_clears_the_transcript_state_but_keeps_the_profile_mode() {
    let mut model = session_model();
    tool(
        &mut model,
        "t1",
        "verify",
        ToolStatus::Completed,
        "candidate",
    );
    model.set_tool_verbosity(Verbosity::Quiet);
    model.toolfold.set("t1", RowState::ShowAll);
    model.toolfold.clear_session();
    assert_eq!(model.toolfold.state_of("t1"), RowState::Collapsed);
    assert_eq!(
        model.toolfold.verbosity(),
        Verbosity::Quiet,
        "the MODE is a profile preference and outlives the transcript"
    );
}

// ---- 8. observed durations --------------------------------------------

#[test]
fn a_tool_duration_is_observed_client_side_and_freezes_when_the_call_lands() {
    let mut model = session_model();
    tool(&mut model, "t1", "bash", ToolStatus::InProgress, "sleep 5");
    model.note_tool_timings();
    let started = model
        .tool_timings
        .get("t1")
        .copied()
        .expect("a live call's start is observed");
    assert_eq!(started.ended_ms, None);

    model.clock_ms += 4_000;
    assert_eq!(started.elapsed_ms(model.clock_ms), 4_000);

    settle(&mut model, "t1", "bash", ToolStatus::Completed, "sleep 5");
    model.note_tool_timings();
    let landed = model
        .tool_timings
        .get("t1")
        .copied()
        .expect("still tracked");
    assert_eq!(landed.ended_ms, Some(model.clock_ms));
    model.clock_ms += 60_000;
    assert_eq!(
        landed.elapsed_ms(model.clock_ms),
        4_000,
        "a settled row's clock is frozen, not still ticking"
    );
}

#[test]
fn a_call_first_seen_already_settled_gets_no_fabricated_duration() {
    let mut model = session_model();
    tool(
        &mut model,
        "t1",
        "bash",
        ToolStatus::Completed,
        "already done",
    );
    model.note_tool_timings();
    assert!(
        !model.tool_timings.contains_key("t1"),
        "a replayed history has no observed start, so the row shows no duration"
    );
}

#[test]
fn clock_skew_renders_zero_rather_than_a_wrapped_figure() {
    let timing = ToolTiming {
        started_ms: 1_000,
        ended_ms: None,
    };
    assert_eq!(timing.elapsed_ms(500), 0);
}

// ---- 9. contrast: dim must be READ, not merely noticed ----------------

fn luminance(rgb: haider_tui::theme::Rgb) -> f64 {
    fn channel(value: u8) -> f64 {
        let value = f64::from(value) / 255.0;
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

fn contrast(a: haider_tui::theme::Rgb, b: haider_tui::theme::Rgb) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

#[test]
fn dim_clears_the_readable_floor_on_every_theme_ground() {
    for key in haider_tui::theme::ThemeKey::ALL {
        let theme = key.theme();
        let ratio = contrast(theme.dim, theme.bg);
        assert!(
            ratio >= 4.5,
            "{}: dim {ratio:.2} is under the 4.5:1 readable floor — collapsed \
             tool rows put the exit code, the line count and the duration on \
             this slot, so it must be READ, not merely noticed",
            theme.label
        );
        assert!(
            contrast(theme.dim, theme.bg) < contrast(theme.text, theme.bg),
            "{}: dim must still read as SECONDARY to body ink",
            theme.label
        );
    }
}

// ---- 10. the tone → theme-slot seam -----------------------------------

#[test]
fn every_tone_resolves_to_a_theme_slot_on_every_theme() {
    let tones = [
        Tone::Body,
        Tone::Meta,
        Tone::Structure,
        Tone::Name,
        Tone::Emphasis,
        Tone::Accent,
        Tone::Ok,
        Tone::Warn,
        Tone::Err,
    ];
    for key in haider_tui::theme::ThemeKey::ALL {
        let theme = key.theme();
        for tone in tones {
            assert!(
                theme.tone_style(tone).fg.is_some(),
                "{}: {tone:?} must resolve to an ink",
                theme.label
            );
        }
    }
    assert_eq!(
        Tone::default(),
        Tone::Meta,
        "an unclassified run is metadata ink, never decoration"
    );
    assert_eq!(
        Segment::new("x", Tone::Ok).width(),
        1,
        "segment width is measured in display cells"
    );
}

// ---- 11. the band's live background-task line (owner addition) --------

use haider_tui::statusline::{Counts, PermissionMode, RowKind, StatusLine};

fn overrides(
    read_only: bool,
    auto_allow: bool,
) -> haider_protocol::session::SessionPermissionOverridesV1 {
    haider_protocol::session::SessionPermissionOverridesV1 {
        read_only,
        allow_writes: true,
        allow_exec: true,
        allow_mobile: false,
        auto_allow,
    }
}

fn shell_row(id: &str) -> haider_rpc::ShellWire {
    haider_rpc::ShellWire {
        id: id.to_owned(),
        kind: haider_rpc::ShellKindWire::Local,
        status: haider_rpc::ShellStatusWire::Running,
        title: format!("gate {id}"),
        cwd_or_host: "/workspace".to_owned(),
        created_at_ms: 1_699_999_880_000,
        last_activity_ms: 1_699_999_990_000,
        bytes_out: 1,
    }
}

fn monitor_row(id: &str, last: Option<&str>) -> haider_rpc::MonitorRegistrationWire {
    haider_rpc::MonitorRegistrationWire {
        monitor_id: id.to_owned(),
        session_id: session_id(),
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
        fire_count: 1,
        next_fire_at_ms: None,
        source_summary: format!("poll {id} · every 60s"),
    }
}

fn tasks_model(shells: usize, monitors: usize) -> AppModel {
    let mut model = session_model();
    model
        .session_permissions
        .insert(session_id(), overrides(false, true));
    model.shells = (0..shells).map(|n| shell_row(&format!("sh-{n}"))).collect();
    model.monitors = (0..monitors)
        .map(|n| {
            monitor_row(
                &format!("mon-{n}"),
                (n == 0).then_some("Checking verify-10-resume exit-code gate"),
            )
        })
        .collect();
    model.monitor_count = model.monitors.len();
    model
}

#[test]
fn the_counts_come_from_daemon_observed_state() {
    let model = tasks_model(6, 14);
    let line = model.status_line();
    assert_eq!(
        line.counts,
        Counts {
            shells: 6,
            monitors: 14,
            agents: 0,
            tasks: 0,
        }
    );
    assert_eq!(
        line.counts.text(),
        "6 shells, 14 monitors",
        "a zero count is omitted entirely — the line never reads `0 agents`"
    );
    assert!(transcript_has(&model, "6 shells, 14 monitors"));
}

#[test]
fn one_of_a_kind_is_singular() {
    assert_eq!(
        Counts {
            shells: 1,
            monitors: 1,
            agents: 1,
            tasks: 1,
        }
        .text(),
        "1 shell, 1 monitor, 1 agent, 1 task"
    );
    assert!(Counts::default().is_empty());
    assert_eq!(Counts::default().text(), "");
}

#[test]
fn the_permission_posture_is_the_one_the_daemon_reported() {
    let mut model = tasks_model(1, 1);
    assert_eq!(model.permission_mode(), Some(PermissionMode::BypassAll));
    assert!(transcript_has(&model, "bypass permissions on"));

    model
        .session_permissions
        .insert(session_id(), overrides(true, true));
    assert_eq!(
        model.permission_mode(),
        Some(PermissionMode::ReadOnly),
        "read_only takes precedence over every allow field, as the daemon applies it"
    );
    assert!(transcript_has(&model, "read-only"));
}

#[test]
fn an_unreported_posture_shows_no_mode_segment_rather_than_a_guess() {
    let mut model = tasks_model(1, 0);
    model.session_permissions.clear();
    assert_eq!(model.permission_mode(), None);
    let text = tf::segments_text(&model.status_line().summary_segments(0));
    assert!(
        !text.contains("permission") && !text.contains("approvals"),
        "the TUI must never echo back the constant it sent at session.create: {text}"
    );
    assert!(text.contains("1 shell"), "{text}");
}

#[test]
fn with_nothing_running_and_no_posture_the_line_spends_no_row() {
    let mut model = session_model();
    model.session_permissions.clear();
    let line = model.status_line();
    assert!(!line.shows());
    assert_eq!(line.height(), 0);
    assert_eq!(
        model.tasks_line_rect.get(),
        None,
        "a line with nothing to say does not take a row from the transcript"
    );
}

#[test]
fn the_task_line_expands_and_collapses_and_defaults_collapsed() {
    let mut model = tasks_model(1, 2);
    assert!(!model.tasks_line_expanded, "collapsed by default");
    assert_eq!(model.status_line().height(), 1);
    assert!(!transcript_has(&model, "○ monitor"));

    model.handle_hit(Hit::TaskLineToggle);
    assert!(model.tasks_line_expanded);
    assert!(transcript_has(&model, "● main"));
    assert!(transcript_has(&model, "○ monitor"));
    assert!(transcript_has(&model, "○ shell"));
    assert!(
        transcript_has(&model, "Checking verify-10-resume exit-code gate"),
        "a monitor's last-event summary is its reported activity"
    );

    model.handle_hit(Hit::TaskLineToggle);
    assert!(!model.tasks_line_expanded);
    assert!(!transcript_has(&model, "● main"));
}

#[test]
fn the_expanded_list_is_bounded_and_reports_what_it_hid() {
    let model = {
        let mut model = tasks_model(6, 14);
        model.toggle_tasks_line();
        model
    };
    let line = model.status_line();
    let (listed, hidden) = line.listed();
    assert_eq!(listed.len(), haider_tui::statusline::MAX_ROWS);
    assert_eq!(hidden, line.rows.len() - haider_tui::statusline::MAX_ROWS);
    assert_eq!(
        line.height(),
        u16::try_from(1 + haider_tui::statusline::MAX_ROWS + 1).expect("small"),
        "one summary row, the bounded list, and the `+K more` row"
    );
    assert!(transcript_has(&model, &format!("+{hidden} more")));
}

#[test]
fn a_row_with_no_reported_activity_shows_what_it_is_not_an_invented_verb() {
    let model = tasks_model(1, 0);
    let line = model.status_line();
    let shell = line
        .rows
        .iter()
        .find(|row| row.kind == RowKind::Shell)
        .expect("the shell row exists");
    assert_eq!(
        shell.activity, None,
        "a registry shell reports bytes and a last-activity instant, never a current line"
    );
    let text = tf::segments_text(&StatusLine::row_segments(shell, 0));
    assert!(text.contains("shell"), "{text}");
    assert!(text.contains("gate sh-0"), "{text}");
    assert!(
        shell.elapsed_ms.is_some(),
        "its start instant IS observed, so the elapsed figure is honest"
    );
}

#[test]
fn the_task_line_sits_directly_under_the_composer_on_every_surface() {
    for screen in [Screen::Session, Screen::Launcher, Screen::Loom] {
        let mut model = tasks_model(2, 3);
        model.screen = screen;
        let _ = rows(&model, 120, 40);
        let band = model
            .band_rect
            .get()
            .unwrap_or_else(|| panic!("{screen:?} must draw a band"));
        let line = model
            .tasks_line_rect
            .get()
            .unwrap_or_else(|| panic!("{screen:?} must draw the task line"));
        assert_eq!(
            line.y,
            band.y + band.height,
            "{screen:?}: the line is directly under the slot — the band owns it, \
             so no surface can place it differently or forget it"
        );
        assert_eq!(line.height, 1, "{screen:?}: collapsed by default");
    }
}

#[test]
fn a_frame_too_short_for_the_expanded_list_collapses_it_before_the_slot_yields() {
    let mut model = tasks_model(6, 14);
    model.toggle_tasks_line();
    let _ = rows(&model, 80, 12);
    let band = model.band_rect.get().expect("the slot never yields");
    assert!(band.height >= 1, "the composer's cursor row is sacred");
    let line = model.tasks_line_rect.get();
    assert!(
        line.is_none_or(|rect| rect.height <= 1),
        "the expanded list is the first thing to yield on a short frame"
    );
}

#[test]
fn a_sub_second_call_reports_milliseconds_rather_than_a_floored_zero() {
    assert_eq!(tf::fmt_duration(412), "412ms");
    assert_eq!(tf::fmt_duration(0), "0ms");
    assert_eq!(tf::fmt_duration(1_000), "1s");
    assert_eq!(tf::fmt_duration(8_200), "8s");
    assert_eq!(tf::fmt_duration(125_000), "2m 5s");
    let facts = RowFacts {
        elapsed_ms: Some(412),
        ..facts("fs_read", "src/app.rs")
    };
    let text = tf::segments_text(&tf::summary_segments(&facts, 0, 0));
    assert!(
        text.ends_with("· 412ms"),
        "a fast call must not read like an unobserved one: {text}"
    );
}

#[test]
fn an_untitled_session_row_does_not_repeat_its_own_label() {
    let model = tasks_model(1, 0);
    let line = model.status_line();
    let main = &line.rows[0];
    assert_eq!(main.kind, RowKind::Main);
    assert_eq!(
        tf::segments_text(&StatusLine::row_segments(main, 0)),
        "  ● main",
        "the reference's row is simply `● main`"
    );
}

#[test]
fn only_an_uneventful_success_folds() {
    // A failing command in the middle of a run of successes breaks it: the
    // non-zero exit code is exactly what the reader needs to see.
    let mut model = session_model();
    command(&mut model, "p0", "echo one", 0);
    command(&mut model, "p1", "false", 1);
    command(&mut model, "p2", "echo three", 0);
    assert!(
        haider_tui::render::foldable_runs(&model.projection, &model.toolfold).is_empty(),
        "a failure is never spoken for by a fold row"
    );
    assert!(transcript_has(&model, "· exit 1"));

    // So does a joined terminal REASON (E8's recovered in-flight retry).
    let mut model = session_model();
    for index in 0..2 {
        tool(
            &mut model,
            &format!("w{index}"),
            "web_fetch",
            ToolStatus::Completed,
            "https://example.test",
        );
    }
    assert_eq!(
        haider_tui::render::foldable_runs(&model.projection, &model.toolfold).len(),
        1,
        "two plain successes DO fold"
    );
    apply(
        &mut model,
        EventPayload::ToolResult {
            call_id: "call-w1".to_owned(),
            result: haider_protocol::tool::BoundedResult {
                preview: "{}".to_owned(),
                truncated: false,
                truncation: None,
                effects: Vec::new(),
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: haider_protocol::tool::ToolResultStatus::Completed,
                reason: Some("transient web_fetch failure — retry 2/2 succeeded".to_owned()),
                presentation: None,
            },
        },
    );
    assert!(
        haider_tui::render::foldable_runs(&model.projection, &model.toolfold).is_empty(),
        "a row carrying a reason shows itself"
    );
    assert!(transcript_has(
        &model,
        "transient web_fetch failure — retry 2/2 succeeded"
    ));
}

#[test]
fn a_tail_cut_at_the_front_never_speaks_with_a_mid_line_fragment() {
    assert_eq!(
        tf::subline_segments_from("ine 0051 — cut\nline 0052 — whole\n", true, 0)
            .map(|segments| tf::segments_text(&segments)),
        Some("    └ line 0052 — whole".to_owned()),
        "the 8 KiB cap opens the tail mid-line; the elbow skips that fragment"
    );
    assert_eq!(
        tf::subline_segments_from("ine 0051 — cut\n", true, 0),
        None,
        "a tail that is nothing BUT a fragment grows no elbow"
    );
    assert!(
        tf::subline_segments_from("whole line\n", false, 0).is_some(),
        "an uncut tail keeps its first line"
    );
}

// ---- 12. round 2: the dedupe, and the Alt-free paths -----------------

#[test]
fn the_counts_have_exactly_one_source_on_the_frame() {
    // Owner DEDUPE ruling: the `▾ subagents` row's `· N shell · N monitor`
    // run is retired; the band's task line is the single source.
    let mut model = tasks_model(2, 3);
    model.chips = Vec::new();
    let painted = rows(&model, 120, 40);
    assert_eq!(
        painted
            .iter()
            .filter(|row| row.contains("2 shells"))
            .count(),
        1,
        "the counts appear once: {painted:#?}"
    );
    assert!(
        !painted.iter().any(|row| row.contains("· 2 shells ·")),
        "the retired right-aligned band-row run is gone"
    );
    assert!(
        !painted.iter().any(|row| row.contains("subagents")),
        "with no subagents that panel owes no row at all"
    );
}

#[test]
fn an_expanded_shell_or_monitor_row_opens_its_overlay() {
    let mut model = tasks_model(1, 1);
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_MONITOR_CONTROL_V1.into());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SHELL_REGISTRY_V1.into());
    model.toggle_tasks_line();
    model.handle_hit(Hit::MonitorStatus);
    assert!(model.monitors_open);
    model.monitors_open = false;
    model.handle_hit(Hit::ShellStatus);
    assert!(
        model.shells_open,
        "the doors 970 owner item 1 hung on the counts follow them here"
    );
}

/// Every ⌥ chord this wave added has a path that survives a terminal
/// configured to swallow Option (tmux · iTerm · Terminal.app — it is the
/// mac compose key). ⌃O for the blanket toggle, typed commands for the rest.
#[test]
fn every_alt_gesture_has_an_alt_free_path() {
    let mut model = session_model();
    tool(
        &mut model,
        "t1",
        "verify",
        ToolStatus::Completed,
        "candidate",
    );
    tool(&mut model, "t2", "grep", ToolStatus::Completed, "hits");

    // ⌥T ← ⌃O
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )));
    assert!(model.toolfold.all_expanded(), "⌃O is ⌥T's twin");
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )));
    assert!(!model.toolfold.all_expanded());

    // ⌥T ← /collapse (idempotent, unlike a flip)
    submit(&mut model, "/collapse expand");
    assert!(model.toolfold.all_expanded());
    submit(&mut model, "/collapse expand");
    assert!(model.toolfold.all_expanded(), "a typed command is absolute");
    submit(&mut model, "/collapse all");
    assert!(!model.toolfold.all_expanded());
    submit(&mut model, "/collapse");
    assert!(model.toolfold.all_expanded(), "bare /collapse toggles");
    submit(&mut model, "/collapse");

    // ⌥N/⌥P ← /collapse next|prev
    submit(&mut model, "/collapse next");
    assert_eq!(model.toolfold.focus(), Some("t2"));
    submit(&mut model, "/collapse prev");
    assert_eq!(model.toolfold.focus(), Some("t1"));

    // ⌥V ← /verbosity
    submit(&mut model, "/verbosity quiet");
    assert_eq!(model.toolfold.verbosity(), Verbosity::Quiet);
    submit(&mut model, "/verbosity");
    assert_eq!(
        model.toolfold.verbosity(),
        Verbosity::Normal,
        "bare /verbosity cycles"
    );

    // ⌥S ← /tasks
    assert!(!model.tasks_line_expanded);
    submit(&mut model, "/tasks");
    assert!(model.tasks_line_expanded);
    submit(&mut model, "/tasks");
    assert!(!model.tasks_line_expanded);
}

#[test]
fn a_mistyped_fallback_argument_is_refused_with_its_vocabulary() {
    let mut model = session_model();
    submit(&mut model, "/verbosity loud");
    let flash = model.flash.clone().expect("a refusal names the vocabulary");
    assert!(
        flash.contains("quiet") && flash.contains("verbose"),
        "{flash}"
    );
    assert_eq!(
        model.toolfold.verbosity(),
        Verbosity::Normal,
        "a refused argument changes nothing"
    );
    submit(&mut model, "/collapse sideways");
    let flash = model.flash.clone().expect("same for /collapse");
    assert!(
        flash.contains("expand") && flash.contains("next"),
        "{flash}"
    );
}

/// Both paths are DOCUMENTED — a chord nobody can discover is not a
/// fallback (owner ruling 4: "document both in the key hints").
#[test]
fn the_key_hints_name_both_paths() {
    for copy in [
        haider_tui::commands::HELP_INTRO_TEXT,
        haider_tui::commands::HELP_TEXT,
    ] {
        let text = copy.join("\n");
        for needle in [
            "⌥T",
            "⌃O",
            "⌥N",
            "⌥V",
            "⌥S",
            "/collapse",
            "/verbosity",
            "/tasks",
        ] {
            assert!(
                text.contains(needle),
                "the key hints must name {needle}: {text}"
            );
        }
    }
}
