//! `--plain` fallback goldens: the projection rendered as stable text lines.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_tui::mock::demo_script;
use haider_tui::plain::{render_plain, status_glyph, status_line, status_word};
use haider_tui::projection::SessionProjection;

#[test]
fn demo_script_renders_the_full_plain_story() {
    let mut projection = SessionProjection::new();
    for payload in &demo_script() {
        projection.apply(payload);
    }
    let text = render_plain(&projection, 200_000, None);
    assert!(text.contains("❯ fix the failing boundary test in haider-store"));
    assert!(text.contains("the boundary check rejects seq 0"));
    assert!(text.contains("⚒ fs_read ✓"));
    assert!(text.contains("± crates/haider-store/src/event_store.rs +4 -1"));
    assert!(text.contains("✓ plan — 3/3 done"));
    assert!(
        !text.contains("todos —"),
        "finished plan is history, not a pinned panel"
    );
    let last_line = text.lines().last().expect("status line");
    assert!(last_line.starts_with("IDLE · 33k tok · "));
    assert!(last_line.ends_with("17% of 200k"));
}

#[test]
fn streaming_agent_text_shows_the_cursor_block() {
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new("i1"),
        item: TurnItem::AgentMessage {
            text: String::new().into(),
        },
    }));
    projection.apply(&EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new("i1"),
        delta: ItemDelta::Text {
            text: "stream".to_owned().into(),
        },
    }));
    assert!(render_plain(&projection, 0, None).contains("stream▮"));
}

#[test]
fn command_blocks_render_output_tail_and_truncation_notice() {
    use base64::Engine as _;
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new("c1"),
        item: TurnItem::CommandExecution {
            call_id: "call".to_owned(),
            command: "cargo test".to_owned(),
            status: ToolStatus::InProgress,
            exit_code: None,
        },
    }));
    let big = vec![b'x'; haider_tui::projection::OUTPUT_TAIL_MAX + 10];
    projection.apply(&EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new("c1"),
        delta: ItemDelta::CommandOutput {
            stream: OutputStream::Stdout,
            chunk_b64: base64::engine::general_purpose::STANDARD.encode(&big),
        },
    }));
    projection.apply(&EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("c1"),
        item: TurnItem::CommandExecution {
            call_id: "call".to_owned(),
            command: "cargo test".to_owned(),
            status: ToolStatus::Failed,
            exit_code: Some(101),
        },
    }));
    let text = render_plain(&projection, 0, None);
    assert!(text.contains("$ cargo test ✗ · exit 101"));
    assert!(!text.contains("! cargo test ✗ · exit 101"));
    assert!(
        !projection.mark_user_command(&haider_protocol::item::UserCommandOriginV1 {
            origin: haider_protocol::item::CommandExecutionOrigin::UserCommand,
            command_item_id: ItemId::new("c1"),
            call_id: "wrong-call".to_owned(),
        }),
        "a mismatched provenance coordinate fails closed"
    );
    assert!(
        projection.mark_user_command(&haider_protocol::item::UserCommandOriginV1 {
            origin: haider_protocol::item::CommandExecutionOrigin::UserCommand,
            command_item_id: ItemId::new("c1"),
            call_id: "call".to_owned(),
        })
    );
    let text = render_plain(&projection, 0, None);
    assert!(text.contains("! cargo test ✗ · exit 101"));
    assert!(text.contains("⋯ earlier output truncated"));
}

#[test]
fn status_glyphs_cover_every_tool_status() {
    assert_eq!(status_glyph(ToolStatus::Pending), "…");
    // Sim ToolRow running glyph (tui.js:3901-3909) — TUI3b parity fix.
    assert_eq!(status_glyph(ToolStatus::InProgress), "◐");
    assert_eq!(status_glyph(ToolStatus::Completed), "✓");
    assert_eq!(status_glyph(ToolStatus::Failed), "✗");
    assert_eq!(status_glyph(ToolStatus::Cancelled), "⊘");
}

#[test]
fn status_words_make_glyph_states_speakable() {
    assert_eq!(status_word(ToolStatus::Pending), "pending");
    assert_eq!(status_word(ToolStatus::InProgress), "running");
    assert_eq!(status_word(ToolStatus::Completed), "completed");
    assert_eq!(status_word(ToolStatus::Failed), "failed");
    assert_eq!(status_word(ToolStatus::Cancelled), "cancelled");
}

#[test]
fn plain_background_status_uses_the_same_explicit_grammar_as_the_band() {
    let projection = SessionProjection::new();
    let background = haider_tui::statusline::StatusLine {
        mode: Some(haider_tui::statusline::PermissionMode::Approvals),
        counts: haider_tui::statusline::Counts {
            shells: 1,
            ..Default::default()
        },
        rows: vec![haider_tui::statusline::Row {
            kind: haider_tui::statusline::RowKind::Shell,
            kind_label: "shell".to_owned(),
            name: "workspace shell".to_owned(),
            activity: None,
            elapsed_ms: Some(1_000),
        }],
        expanded: true,
        page: 0,
    };
    let plain = haider_tui::plain::render_plain_with_status(
        &projection,
        0,
        None,
        &Default::default(),
        &background,
    );
    assert!(plain.contains("background status: approvals on; 1 shell"));
    assert!(plain.contains("shell — name: workspace shell; elapsed 1s"));
    assert!(plain.ends_with("IDLE · 0 tok\n"));
}

#[test]
fn semantic_tool_rows_keep_tool_specific_context_compact() {
    use haider_tui::toolfold::semantic_summary;
    use serde_json::json;

    assert_eq!(
        semantic_summary(
            "bash",
            &json!({"command":"cargo test", "description":"run checks"})
        ),
        "cargo test — run checks"
    );
    assert_eq!(
        semantic_summary(
            "fs_read",
            &json!({"path":"src/lib.rs", "line_range":"10-20"})
        ),
        "src/lib.rs · 10-20"
    );
    assert_eq!(
        semantic_summary(
            "grep",
            &json!({"pattern":"needle", "path":"src", "matches":2})
        ),
        "\"needle\" in src · 2 matches"
    );
    assert_eq!(
        semantic_summary("task", &json!({"label":"verify", "state":"running"})),
        "verify · running"
    );
    assert_eq!(
        semantic_summary("fs_read", &json!({"path":"/Users/bob/private"})),
        "/Users/<user>/private"
    );
    assert_eq!(
        semantic_summary("fs_edit", &json!({"path":"/Users/bob/private"})),
        "/Users/<user>/private"
    );
}

#[test]
fn plain_tool_paths_use_the_shared_account_projection() {
    use serde_json::json;
    for (name, args, expected) in [
        (
            "fs_list",
            json!({"path": "/Users/bob/private"}),
            "/Users/<user>/private",
        ),
        (
            "glob",
            json!({"path": "/Users/bob/private"}),
            "/Users/<user>/private",
        ),
        ("ls", json!({"path": "/home/carol"}), "/home/<user>"),
        (
            "fs_search",
            json!({"path": "/Users/bob/private"}),
            "/Users/<user>/private",
        ),
        (
            "custom_tool",
            json!({"path": "/Users/bob/private"}),
            "/Users/<user>/private",
        ),
        ("fs_list", json!({"path": "src"}), "src"),
        (
            "custom_tool",
            json!({"cwd": "/Users/bob/repo"}),
            "/Users/<user>/repo",
        ),
        (
            "custom_tool",
            json!({"file_path": "/Users/bob/repo/x"}),
            "/Users/<user>/repo/x",
        ),
        (
            "fs_search",
            json!({"query": "x", "glob": "/Users/bob/**"}),
            "\"x\" /Users/<user>/**",
        ),
        (
            "fs_search",
            json!({"query": "x", "include": "/Users/bob/**"}),
            "\"x\" /Users/<user>/**",
        ),
        (
            "fs_search",
            json!({"query": "x", "include": "src/**"}),
            "\"x\" src/**",
        ),
        (
            "glob",
            json!({"pattern": "/Users/bob/**"}),
            "/Users/<user>/**",
        ),
        (
            "fs_search",
            json!({"query": "x", "glob": r"C:\Users\bob\**"}),
            "\"x\" C:/Users/<user>/**",
        ),
        (
            "fs_search",
            json!({"query": "x", "include": "../src/**"}),
            "\"x\" ../src/**",
        ),
        (
            "fs_read",
            json!({"path": r"C:/w/..\Users\bob\x"}),
            "C:/Users/<user>/x",
        ),
        (
            "custom_tool",
            json!({"cwd": r"C:/w/sub\..\..\Users\bob\x"}),
            "C:/Users/<user>/x",
        ),
    ] {
        let mut projection = SessionProjection::new();
        projection.apply(&EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("path-call"),
            item: TurnItem::ToolCall {
                call_id: "path-call".into(),
                name: name.into(),
                args,
                status: ToolStatus::InProgress,
            },
        }));
        let plain = render_plain(&projection, 0, None);
        assert!(
            plain.contains(&format!("⚒ {name} ◐ · {expected}")),
            "{plain}"
        );
    }
}

#[test]
fn reduced_motion_freezes_only_the_spinner_frame() {
    use haider_tui::toolfold::{SPINNER, spinner_frame_for};
    assert_eq!(spinner_frame_for(2, false), SPINNER[2]);
    assert_eq!(spinner_frame_for(2, true), SPINNER[0]);
    assert_eq!(spinner_frame_for(99, true), SPINNER[0]);
}

#[test]
fn status_line_without_window_omits_the_meter() {
    let projection = SessionProjection::new();
    assert_eq!(status_line(&projection, 0), "IDLE · 0 tok");
}

#[test]
fn plain_session_paths_keep_workspace_context_for_all_path_keys() {
    use haider_tui::{app::AppModel, runtime::run_demo_plain, toolview::header_args};
    use serde_json::json;
    for (workspace, path) in [
        (
            "C:/Users/alice/project",
            "C:/Users/alice/project/src/probe.rs",
        ),
        (
            "C:/Users/alice/project",
            r"c:\users\ALICE\PROJECT\src\probe.rs",
        ),
        ("/opt/project", "/opt/project/src/probe.rs"),
        ("D:/repo", "D:/repo/src/probe.rs"),
        (r"\\srv\share\repo", r"\\srv\share\repo\src\probe.rs"),
        (
            r"\\?\C:\Users\alice\project",
            r"\\?\C:\Users\alice\project\src\probe.rs",
        ),
        (r"\\.\C:\repo", r"\\.\C:\repo\src\probe.rs"),
    ] {
        for key in [
            "path",
            "file",
            "file_path",
            "cwd",
            "directory",
            "workspace",
            "root",
        ] {
            for name in ["fs_read", "fs_list", "fs_edit", "custom_tool"] {
                let mut model = AppModel::new();
                model.cwd = workspace.into();
                model.session_workspace_cwd = Some(workspace.into());
                model.display_homes = vec!["C:/Users/alice".into()];
                let args = json!({key: path});
                let expected = header_args(name, &args, &model.tool_path_context());
                assert_eq!(expected, "src/probe.rs");
                model
                    .projection
                    .apply(&EventPayload::Item(ItemEvent::Started {
                        item_id: ItemId::new("context-probe"),
                        item: TurnItem::ToolCall {
                            call_id: "context-probe".into(),
                            name: name.into(),
                            args,
                            status: ToolStatus::InProgress,
                        },
                    }));
                let plain = run_demo_plain(model);
                assert!(
                    plain
                        .lines()
                        .any(|line| line.starts_with(&format!("⚒ {name} ◐ · {expected}"))),
                    "{name} {key} {workspace}: {plain}"
                );
            }
        }
    }
}

#[test]
fn plain_pattern_scope_uses_session_context() {
    use haider_tui::{app::AppModel, runtime::run_demo_plain};
    use serde_json::json;
    for (name, args, expected) in [
        (
            "fs_read",
            json!({"file_path":"C:/repo/src/key_probe.rs", "file":"C:/Users/bob/other"}),
            "src/key_probe.rs",
        ),
        (
            "custom_tool",
            json!({"cwd":"C:/repo", "root":"C:/repo/src", "description":"inspect"}),
            ".",
        ),
        (
            "fs_search",
            json!({"query":"needle", "cwd":"C:/repo", "root":"C:/repo/src"}),
            "\"needle\" in src",
        ),
        (
            "fs_search",
            json!({"root":"C:/repo/src", "description":"inspect"}),
            "src",
        ),
        (
            "fs_search",
            json!({"query":"needle", "root":"C:/repo/src"}),
            "\"needle\" in src",
        ),
        (
            "fs_search",
            json!({"query":"needle", "path":"C:/repo/src", "glob":"C:/repo/src/**"}),
            "\"needle\" in src src/**",
        ),
        (
            "fs_search",
            json!({"query":"needle", "include":"C:/repo/src/**"}),
            "\"needle\" src/**",
        ),
        (
            "glob",
            json!({"pattern":"C:/repo/src/**", "path":"C:/repo/src"}),
            "src/** in src",
        ),
        ("fs_glob", json!({"query":"!C:/repo/src/**"}), "!src/**"),
        ("fs_read", json!({"path":"C:/repo"}), "."),
        (
            "custom_tool",
            json!({"cwd":"C:/repo", "description":"inspect workspace", "desc":"inspect", "action":"inspect"}),
            ".",
        ),
        (
            "agent_spawn",
            json!({"cwd":"C:/repo", "label":"verify"}),
            "verify in .",
        ),
        (
            "shell",
            json!({"cwd":"C:/repo", "description":"inspect workspace"}),
            ".",
        ),
        (
            "fs_read",
            json!({"path":"C:/Users/alice/notes.md"}),
            "~/notes.md",
        ),
        (
            "fs_read",
            json!({"path":"C:/Users/bob/notes.md"}),
            "C:/Users/<user>/notes.md",
        ),
        (
            "fs_search",
            json!({"query":"/Users/bob", "path":"src"}),
            "\"/Users/bob\" in src",
        ),
    ] {
        let mut model = AppModel::new();
        model.cwd = "C:/repo".into();
        model.display_homes = vec!["C:/Users/alice".into()];
        model
            .projection
            .apply(&EventPayload::Item(ItemEvent::Started {
                item_id: ItemId::new("scope-probe"),
                item: TurnItem::ToolCall {
                    call_id: "scope-probe".into(),
                    name: name.into(),
                    args,
                    status: ToolStatus::InProgress,
                },
            }));
        let plain = run_demo_plain(model);
        assert!(
            plain
                .lines()
                .any(|line| line.starts_with(&format!("⚒ {name} ◐ · {expected}"))),
            "{name} {expected}: {plain}"
        );
    }
}
