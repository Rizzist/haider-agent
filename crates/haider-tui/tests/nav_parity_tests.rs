//! Navigation parity contracts for transcript search and workspace mentions.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::ffi::OsString;
use std::fs;

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_tui::app::{AppEvent, AppModel, MentionCompletion, Screen};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;

#[cfg(unix)]
fn non_utf8_name() -> OsString {
    use std::os::unix::ffi::OsStringExt;

    OsString::from_vec(b"bad-\xff.txt".to_vec())
}

#[cfg(windows)]
fn non_utf8_name() -> OsString {
    use std::os::windows::ffi::OsStringExt;

    // A lone surrogate is the Windows equivalent of an invalid UTF-8 byte:
    // it exercises lossy filename handling without assuming UTF-16 validity.
    OsString::from_wide(&[
        'b' as u16, 'a' as u16, 'd' as u16, '-' as u16, 0xD800, '.' as u16, 't' as u16, 'x' as u16,
        't' as u16,
    ])
}

fn session_model() -> AppModel {
    let mut model = common::launcher_model();
    model.mode = haider_tui::app::RuntimeMode::Live;
    model.sessions.clear();
    let id = haider_protocol::ids::SessionId::new("nav-session");
    model.upsert_live_session(&id);
    model.open_session(&id);
    model.requests.clear();
    model.screen = Screen::Session;
    model
}

fn agent(model: &mut AppModel, id: &str, text: &str) {
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item: TurnItem::AgentMessage {
                text: text.to_owned().into(),
            },
        }));
}

#[test]
fn workspace_mentions_index_files_and_tab_accepts_the_selected_path() {
    let root = tempfile::tempdir().expect("temporary workspace");
    fs::create_dir(root.path().join("src")).expect("source directory");
    fs::write(root.path().join("src/main.rs"), "fn main() {}").expect("source file");
    fs::write(root.path().join("README.md"), "# readme").expect("readme");

    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.session_workspace_cwd = Some(root.path().to_string_lossy().into_owned());
    model.handle(common::key(KeyCode::Char('@')));
    for character in "src/".chars() {
        model.handle(common::key(KeyCode::Char(character)));
    }

    let completion = model
        .mention_completion
        .as_ref()
        .expect("@ opens file completion");
    assert!(
        completion
            .candidates
            .iter()
            .any(|path| path == "src/main.rs")
    );
    assert!(!completion.candidates.iter().any(|path| path == "README.md"));

    model.handle(common::key(KeyCode::Tab));
    assert_eq!(model.composer.text(), "@src/main.rs");
    assert!(model.mention_completion.is_none());
}

#[test]
fn ctrl_f_opens_a_transcript_search_without_mutating_the_composer() {
    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.composer.insert_str("draft");
    model.handle(common::ctrl(KeyCode::Char('f')));
    assert!(model.transcript_search.is_some());
    assert_eq!(model.composer.text(), "draft");

    model.handle(common::key(KeyCode::Char('x')));
    assert_eq!(model.transcript_search.as_ref().unwrap().query, "x");
    model.handle(common::key(KeyCode::Esc));
    assert!(model.transcript_search.is_none());

    // The control chord is explicit; a bare `f` remains normal composer input.
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::NONE,
    )));
    assert_eq!(model.composer.text(), "draftf");
}

#[test]
fn transcript_search_preserves_arrival_order_and_wraps_selection() {
    let mut model = session_model();
    agent(&mut model, "first", "needle one");
    agent(&mut model, "second", "needle two");
    agent(&mut model, "last", "unrelated");

    model.handle(common::ctrl(KeyCode::Char('f')));
    for character in "needle".chars() {
        model.handle(common::key(KeyCode::Char(character)));
    }
    let search = model.transcript_search.as_ref().expect("search open");
    assert_eq!(
        search.matches,
        vec![0, 1],
        "matches stay in transcript order"
    );
    assert_eq!(search.selected, 0);

    model.handle(common::key(KeyCode::Enter));
    assert_eq!(model.transcript_search.as_ref().unwrap().selected, 1);
    model.handle(common::key(KeyCode::Enter));
    assert_eq!(model.transcript_search.as_ref().unwrap().selected, 0);
    model.handle(common::key(KeyCode::Up));
    assert_eq!(model.transcript_search.as_ref().unwrap().selected, 1);
}

#[test]
fn wide_unicode_search_rows_render_and_match() {
    let mut model = session_model();
    agent(&mut model, "wide", "界界 — needle — 日本語");
    model.handle(common::ctrl(KeyCode::Char('f')));
    for character in "界界".chars() {
        model.handle(common::key(KeyCode::Char(character)));
    }
    assert_eq!(model.transcript_search.as_ref().unwrap().matches, vec![0]);

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            haider_tui::render::render(&model, frame);
        })
        .expect("render");
    assert_eq!(model.transcript_search.as_ref().unwrap().query, "界界");
}

#[test]
fn search_does_not_steal_bounded_capture_paging() {
    let mut model = session_model();
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("capture"),
            item: TurnItem::CommandExecution {
                call_id: "call-capture".to_owned(),
                command: "capture".to_owned(),
                status: ToolStatus::InProgress,
                exit_code: None,
            },
        }));
    let output = (0..40)
        .map(|n| format!("capture line {n}\n"))
        .collect::<String>();
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Delta {
            item_id: ItemId::new("capture"),
            delta: ItemDelta::CommandOutput {
                stream: OutputStream::Stdout,
                chunk_b64: base64::engine::general_purpose::STANDARD.encode(output.as_bytes()),
            },
        }));
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("capture"),
            item: TurnItem::CommandExecution {
                call_id: "call-capture".to_owned(),
                command: "capture".to_owned(),
                status: ToolStatus::Completed,
                exit_code: Some(0),
            },
        }));
    model.move_tool_focus(true);
    model.cycle_focused_tool_row();
    model.handle(common::ctrl(KeyCode::Char('f')));
    let before = model.toolfold.scroll_of("capture");
    model.handle(common::key(KeyCode::PageDown));
    assert_eq!(
        model.toolfold.scroll_of("capture"),
        before,
        "search owns PageDown"
    );
    model.handle(common::key(KeyCode::Esc));
    model.handle(common::key(KeyCode::PageDown));
    let after = model.toolfold.scroll_of("capture");
    assert_eq!(after, before + 9, "one bounded paging path remains");
}

#[test]
fn mention_completion_handles_empty_query_huge_directory_and_non_utf8_name() {
    let root = tempfile::tempdir().expect("temporary workspace");
    for index in 0..300 {
        fs::write(root.path().join(format!("file-{index:03}.txt")), "x").expect("file");
    }
    let non_utf8 = non_utf8_name();
    let non_utf8_created = fs::write(root.path().join(&non_utf8), "x").is_ok();
    if !non_utf8_created {
        assert!(non_utf8.to_string_lossy().contains('�'));
    }

    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.session_workspace_cwd = Some(root.path().to_string_lossy().into_owned());
    model.handle(common::key(KeyCode::Char('@')));
    let completion = model
        .mention_completion
        .as_ref()
        .expect("empty query opens completion");
    assert!(
        completion.candidates.len() <= 64,
        "completion remains bounded"
    );

    assert!(!completion.scan_truncated);
    let mut expected = (0..300)
        .map(|index| format!("file-{index:03}.txt"))
        .collect::<Vec<_>>();
    if non_utf8_created {
        expected.push(non_utf8.to_string_lossy().into_owned());
    }
    expected.sort();
    expected.truncate(64);
    assert_eq!(completion.candidates, expected, "lexical first 64 paths");

    // Query every distinct file in this unchanged directory. Regardless of
    // readdir order, a scan that samples only 256 raw entries must miss some
    // of these 300 matches. A single chosen filename can hide that bug on
    // filesystems that happen to enumerate it early.
    for index in 0..300 {
        let filename = format!("file-{index:03}.txt");
        model.composer.set_text(format!("@file-{index:03}.tx"));
        model.handle(common::key(KeyCode::Char('t')));
        assert_eq!(
            model.mention_completion.as_ref().unwrap().candidates,
            vec![filename],
            "every matching file must be reachable, regardless of directory order"
        );
    }

    model.handle(common::key(KeyCode::Esc));
    model.composer.set_text("");
    model.handle(common::key(KeyCode::Char('@')));
    for character in "bad-".chars() {
        model.handle(common::key(KeyCode::Char(character)));
    }
    let completion = model
        .mention_completion
        .as_ref()
        .expect("non-UTF8 query completion");
    if non_utf8_created {
        assert!(
            completion
                .candidates
                .iter()
                .any(|candidate| candidate.contains("bad-"))
        );
    } else {
        assert!(non_utf8.to_string_lossy().contains('�'));
    }
}

#[test]
fn mention_scan_ceiling_is_reported_even_without_matches() {
    let root = tempfile::tempdir().expect("temporary workspace");
    // Pin the safety ceiling separately from the 64-result popup bound.
    for index in 0..16_384 {
        fs::write(root.path().join(format!("file-{index:05}.txt")), "").expect("file");
    }
    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.session_workspace_cwd = Some(root.path().to_string_lossy().into_owned());
    model.composer.set_text("@absen");
    model.handle(common::key(KeyCode::Char('t')));
    let completion = model.mention_completion.as_ref().expect("completion");
    assert!(completion.candidates.is_empty());
    assert!(
        !completion.scan_truncated,
        "exactly at the ceiling is complete"
    );

    fs::write(root.path().join("one-more.txt"), "").expect("file past ceiling");
    model.handle(common::key(KeyCode::Char('x')));
    let completion = model.mention_completion.as_ref().expect("completion");
    assert!(completion.candidates.is_empty());
    assert!(
        completion.scan_truncated,
        "incomplete scan must be disclosed"
    );

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).expect("terminal");
    terminal
        .draw(|frame| {
            haider_tui::render::render(&model, frame);
        })
        .expect("render");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Workspace scan truncated"));

    model.handle(common::key(KeyCode::Tab));
    assert_eq!(
        model.composer.text(),
        "@absentx",
        "marker is not a candidate"
    );
}

#[test]
fn truncated_mentions_render_query_and_marker_without_matches() {
    let mut model = session_model();
    model.composer.set_text("@absentx");
    model.mention_completion = Some(MentionCompletion {
        query: "absentx".to_owned(),
        scan_truncated: true,
        ..MentionCompletion::default()
    });

    assert_truncated_completion_rows(&model, "@absentx", &[]);
}

#[test]
fn truncated_mentions_render_query_marker_and_candidate_window() {
    let mut model = session_model();
    model.composer.set_text("@file-");
    model.mention_completion = Some(MentionCompletion {
        query: "file-".to_owned(),
        candidates: (0..5).map(|index| format!("file-{index:03}.txt")).collect(),
        scan_truncated: true,
        ..MentionCompletion::default()
    });

    assert_truncated_completion_rows(
        &model,
        "@file-",
        &[
            "▸ @file-000.txt",
            "@file-001.txt",
            "@file-002.txt",
            "@file-003.txt",
        ],
    );
    for _ in 0..4 {
        model.handle(common::key(KeyCode::Down));
    }
    assert_truncated_completion_rows(
        &model,
        "@file-",
        &[
            "@file-001.txt",
            "@file-002.txt",
            "@file-003.txt",
            "▸ @file-004.txt",
        ],
    );
}

fn assert_truncated_completion_rows(model: &AppModel, query: &str, candidates: &[&str]) {
    for (width, height) in [(100, 30), (40, 20)] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("terminal");
        terminal
            .draw(|frame| {
                haider_tui::render::render(model, frame);
            })
            .expect("render");
        let rows = terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        // Match the composer sigil with the query, so a candidate containing
        // the same prefix cannot stand in for a visible draft.
        let query_row = rows
            .iter()
            .position(|row| row.contains(&format!("❯ {query}")))
            .unwrap_or_else(|| panic!("query hidden at {width}x{height}:\n{}", rows.join("\n")));
        assert_eq!(rows[query_row + 1], "… Workspace scan truncated");
        for (offset, candidate) in candidates.iter().enumerate() {
            assert_eq!(&rows[query_row + 2 + offset], candidate);
        }
    }
}

#[test]
fn resume_session_list_conventions_are_explicit_and_filterable() {
    let mut model = common::launcher_model();
    model.mode = haider_tui::app::RuntimeMode::Live;
    model.sessions.clear();
    let first = haider_protocol::ids::SessionId::new("session-alpha");
    let second = haider_protocol::ids::SessionId::new("session-beta");
    model.upsert_live_session(&first);
    model.upsert_live_session(&second);
    model
        .sessions
        .iter_mut()
        .find(|row| row.id == first)
        .unwrap()
        .title = Some("Deploy API".to_owned());
    model
        .sessions
        .iter_mut()
        .find(|row| row.id == first)
        .unwrap()
        .dir = "/work/api".to_owned();
    model
        .sessions
        .iter_mut()
        .find(|row| row.id == first)
        .unwrap()
        .model_short = "gpt-5.6".to_owned();
    let second_row = model
        .sessions
        .iter_mut()
        .find(|row| row.id == second)
        .unwrap();
    second_row.title = Some("Review UI".to_owned());
    second_row.dir = "/work/beta".to_owned();
    second_row.model_short = "qwen".to_owned();
    model.enter_sessions();
    assert_eq!(
        model.screen,
        Screen::Sessions,
        "/resume opens the full list"
    );
    assert_eq!(model.session_browser_rows().len(), 2);
    model.handle(common::key(KeyCode::Char('d')));
    assert_eq!(
        model.session_browser_rows().len(),
        1,
        "title/dir/model/id search is live"
    );
    assert_eq!(model.session_browser_rows()[0].id, first);
    model.handle(common::key(KeyCode::Enter));
    assert_eq!(
        model.active_session_id(),
        Some(&first),
        "Enter opens the selected row"
    );
    assert!(
        haider_tui::commands::HELP_TEXT
            .iter()
            .any(|line| line.contains("/sessions"))
    );
}
