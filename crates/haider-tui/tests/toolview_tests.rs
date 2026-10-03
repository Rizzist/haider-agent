//! 973-tui-toolview — the PURE half of the Claude-Code-style tool rows:
//! verbs, path shortening, one-line commands, the diff model, the `⎿`
//! result line, fold phrases, display rows and the diff/text contrast floor.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::tool::EditSpanV1;
use haider_tui::toolfold::{Segment, Tone, segments_text};
use haider_tui::toolview::{
    self as tv, DetailFacts, DiffKind, PathContext, ResultFacts, RowRole, Settle, ToolKind,
};
use serde_json::json;

fn paths() -> PathContext {
    PathContext {
        workspace: Some("C:/Users/alice/dev/haider".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    }
}

fn result_text(facts: &ResultFacts<'_>) -> Option<String> {
    tv::result_segments(facts, 0).map(|segments| segments_text(&segments))
}

// ---- 1. vocabulary -----------------------------------------------------

#[test]
fn every_tool_family_reads_as_a_human_verb() {
    for (name, verb) in [
        ("fs_read", "Read"),
        ("read", "Read"),
        ("fs_write", "Write"),
        ("write", "Write"),
        ("fs_edit", "Edit"),
        ("edit", "Edit"),
        ("fs_search", "Search"),
        ("grep", "Search"),
        ("fs_glob", "Glob"),
        ("process_exec", "Bash"),
        ("bash", "Bash"),
        ("ssh_shell", "Bash"),
        ("web_fetch", "Fetch"),
        ("web_search", "Web Search"),
        ("Monitor", "Monitor"),
        ("todo_write", "todo_write"),
    ] {
        assert_eq!(tv::verb(name), verb, "{name}");
    }
    assert_eq!(tv::tool_kind("fs_glob"), ToolKind::Search);
    assert_eq!(tv::tool_kind("spawn_subagent"), ToolKind::Other);
}

#[test]
fn paths_shorten_to_the_workspace_then_home_and_never_across_a_component() {
    let paths = paths();
    assert_eq!(
        paths.shorten("C:/Users/alice/dev/haider/state/973-LANES.md"),
        "state/973-LANES.md"
    );
    assert_eq!(paths.shorten("C:/Users/alice/dev/haider"), ".");
    assert_eq!(
        paths.shorten("C:/Users/alice/Developer/notes.md"),
        "~/Developer/notes.md"
    );
    assert_eq!(paths.shorten("C:/Users/alice"), "~");
    // Component-aware: a sibling that merely shares a prefix is NOT inside.
    assert_eq!(
        paths.shorten("C:/Users/alice2/x.rs"),
        "C:/Users/<user>/x.rs"
    );
    assert_eq!(
        paths.shorten("C:/Users/alice/dev/haider-old/x.rs"),
        "~/dev/haider-old/x.rs"
    );
    assert_eq!(paths.shorten("/etc/hosts"), "/etc/hosts");
    assert_eq!(paths.shorten("src/app.rs"), "src/app.rs", "relative stays");
    assert_eq!(
        paths.shorten("../bob/secret.txt"),
        "../bob/secret.txt",
        "relative traversal has no authoritative tool cwd"
    );
    // A trailing slash on the root changes nothing.
    let slashed = PathContext {
        workspace: Some("/w/".to_owned()),
        homes: Vec::new(),
    };
    assert_eq!(slashed.shorten("/w/a.rs"), "a.rs");
    // No context shortens nothing.
    assert_eq!(PathContext::default().shorten("/w/a.rs"), "/w/a.rs");
}

#[test]
fn tool_headers_share_the_home_and_other_account_projection() {
    let context = PathContext {
        workspace: Some("C:/Users/alice/dev/haider".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    };
    for tool in ["fs_read", "fs_write", "fs_edit"] {
        assert_eq!(
            tv::header_args(tool, &json!({"path": "/Users/bob/private"}), &context),
            "/Users/<user>/private",
            "{tool}"
        );
        assert_eq!(
            tv::header_args(
                tool,
                &json!({"path": r"\\?\C:\Users\alice\notes.md"}),
                &context
            ),
            "~/notes.md",
            "{tool}"
        );
    }
    assert_eq!(
        tv::header_args(
            "fs_search",
            &json!({"pattern": "needle", "root": "/Users/bob/repo"}),
            &context,
        ),
        "\"needle\" in /Users/<user>/repo"
    );
    assert_eq!(
        tv::header_args("custom_tool", &json!({"cwd": "/Users/bob/repo"}), &context),
        "/Users/<user>/repo"
    );
    assert_eq!(
        context.shorten("C:/Users/alice/dev/haider/src/lib.rs"),
        "src/lib.rs"
    );
    assert_eq!(
        context.shorten("C:/Users/alice/dev/haider/../../../bob/private"),
        "C:/Users/<user>/private"
    );
    assert_eq!(
        context.shorten(r"\\.\pipe\haider-0123"),
        r"\\.\pipe\haider-0123"
    );
}

#[test]
fn workspace_root_cannot_shorten_away_another_account_mask() {
    for workspace in ["/", "/Users", "/Users/"] {
        let context = PathContext {
            workspace: Some(workspace.to_owned()),
            homes: vec!["C:/Users/alice".into()],
        };
        assert_eq!(
            context.shorten("/Users/bob/private"),
            "/Users/<user>/private"
        );
        assert_eq!(
            tv::header_args("fs_read", &json!({"path": "/Users/bob/private"}), &context),
            "/Users/<user>/private"
        );
        assert_eq!(context.shorten("C:/Users/alice/private"), "~/private");
    }
    let windows = PathContext {
        workspace: Some("C:/Users".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    };
    assert_eq!(
        windows.shorten("C:/Users/bob/private"),
        "C:/Users/<user>/private"
    );
    assert_eq!(windows.shorten("C:/Users/alice/private"), "~/private");
}

#[test]
fn account_shaped_workspace_keeps_safe_relative_labels() {
    for (workspace, path) in [
        ("/Users/Shared/proj", "/Users/Shared/proj/src/x.rs"),
        ("/home/build/repo", "/home/build/repo/src/x.rs"),
        ("C:/Users/Public/proj", "C:/Users/Public/proj/src/x.rs"),
    ] {
        let context = PathContext {
            workspace: Some(workspace.to_owned()),
            homes: vec!["C:/Users/alice".into()],
        };
        assert_eq!(context.shorten(workspace), ".", "{workspace}");
        assert_eq!(context.shorten(path), "src/x.rs", "{workspace}");
        assert_eq!(
            context.shorten(&format!("{workspace}/../bob/private")),
            if workspace.starts_with("C:") {
                "C:/Users/<user>/bob/private"
            } else if workspace.starts_with("/Users") {
                "/Users/<user>/bob/private"
            } else {
                "/home/<user>/bob/private"
            },
            "a parent traversal cannot gain a workspace label"
        );
    }
    let shared = PathContext {
        workspace: Some("/Users/Shared/proj".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    };
    assert_eq!(
        shared.shorten("/Users/Shared/proj-old/src/x.rs"),
        "/Users/<user>/proj-old/src/x.rs",
        "a sibling prefix is outside the workspace"
    );
}

#[test]
fn mixed_windows_separators_cannot_escape_workspace_label() {
    for (workspace, path) in [
        ("C:/w", r"C:/w/..\Users\bob\x"),
        ("C:/w", r"C:/w/sub\..\..\Users\bob\x"),
        ("C:/Users/alice/dev", r"C:/Users/alice/dev/..\..\bob\x"),
        (r"C:\w", r"C:\w\..\Users\bob\x"),
        (r"\\?\C:\w", r"\\?\C:\w\..\Users\bob\x"),
        (r"\\srv\share\w", r"\\srv\share\w\..\Users\bob\x"),
    ] {
        let context = PathContext {
            workspace: Some(workspace.to_owned()),
            homes: vec!["C:/Users/alice".into()],
        };
        let expected = if path.starts_with(r"\\srv") {
            "//srv/share/Users/<user>/x"
        } else {
            "C:/Users/<user>/x"
        };
        assert_eq!(context.shorten(path), expected, "{workspace}: {path}");
        assert_eq!(
            tv::header_args("fs_read", &json!({"path": path}), &context),
            expected,
            "header: {path}"
        );
    }
}

#[test]
fn dot_segments_never_become_workspace_labels() {
    let context = PathContext {
        workspace: Some("C:/w".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    };
    for path in [
        "C:/w/./src/x.rs",
        r"C:/w/.\src\x.rs",
        "C:/w/sub/../src/x.rs",
        r"C:/w/sub\..\src\x.rs",
    ] {
        assert_eq!(context.shorten(path), "C:/w/src/x.rs", "{path}");
    }
}

#[test]
fn windows_dot_and_space_segments_never_become_workspace_labels() {
    use std::path::Path;

    let context = PathContext {
        workspace: Some("C:/w".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    };
    for path in [
        "C:/w/.. /Users/bob/x",
        "C:/w/sub/.. /.. /Users/bob/x",
        "C:/w/.. . /Users/bob/x",
        "C:/w/.../Users/bob/x",
        r"\\?\C:\w\.. \Users\bob\x",
    ] {
        assert_eq!(
            haider_client::launch_origin::workspace_relative_display(
                Path::new(path),
                Path::new("C:/w")
            ),
            None,
            "{path}"
        );
        let label = context.shorten(path);
        assert!(label.contains("<user>"), "{path}: {label}");
        assert!(!label.contains("bob"), "{path}: {label}");
        assert!(!label.starts_with(".."), "{path}: {label}");
    }
    for path in ["C:/w/. /Users/bob/x", "C:/w/ /Users/bob/x"] {
        assert_eq!(
            haider_client::launch_origin::workspace_relative_display(
                Path::new(path),
                Path::new("C:/w")
            ),
            None,
            "{path}"
        );
        assert_eq!(context.shorten(path), "C:/w/Users/bob/x", "{path}");
    }
    // Ordinary leading-dot names still label a file below the workspace.
    assert_eq!(context.shorten("C:/w/..file/x"), "..file/x");
    assert_eq!(context.shorten("C:/w/.config/x"), ".config/x");
}

#[cfg(windows)]
#[test]
fn relative_windows_dot_and_space_names_remain_lexical() {
    let context = PathContext {
        workspace: Some("C:/w".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    };
    for path in [".../notes", ".. /notes", ". /notes"] {
        assert_eq!(context.shorten(path), path);
    }
}

#[test]
fn equivalent_windows_spellings_keep_safe_workspace_labels() {
    for (workspace, path) in [
        (r"C:\w", r"C:\w\src\x.rs"),
        ("C:/w", r"C:\w\src\x.rs"),
        ("C:/w", "c:/W/src/x.rs"),
        (r"\\?\C:\w", r"C:\w\src\x.rs"),
        ("C:/w", r"\\?\C:\w\src\x.rs"),
        (r"\\srv\share\w", r"\\SRV\SHARE\W\src\x.rs"),
        (r"\\?\UNC\srv\share\w", r"\\srv\share\w\src\x.rs"),
    ] {
        let context = PathContext {
            workspace: Some(workspace.to_owned()),
            homes: vec!["C:/Users/alice".into()],
        };
        assert_eq!(context.shorten(path), "src/x.rs", "{workspace}: {path}");
    }
    let own_home = PathContext {
        workspace: Some("C:/w".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    };
    assert_eq!(own_home.shorten("C:/Users/alice"), "~");
    assert_eq!(own_home.shorten("/etc/hosts"), "/etc/hosts");
    assert_eq!(own_home.shorten("src/x.rs"), "src/x.rs");
}

#[cfg(target_os = "macos")]
#[test]
fn canonical_tmp_alias_keeps_the_workspace_label() {
    let context = PathContext {
        workspace: Some("/tmp/winpath-973-alias".to_owned()),
        homes: Vec::new(),
    };
    assert_eq!(
        context.shorten("/private/tmp/winpath-973-alias/src/x.rs"),
        "src/x.rs"
    );
}

#[test]
fn absolute_search_patterns_use_the_account_projection() {
    let context = PathContext {
        workspace: Some("/w".to_owned()),
        homes: vec!["C:/Users/alice".into()],
    };
    for args in [
        json!({"query": "x", "glob": "/Users/bob/**"}),
        json!({"query": "x", "include": "/Users/bob/**"}),
    ] {
        assert_eq!(
            tv::header_args("fs_search", &args, &context),
            "\"x\" /Users/<user>/**"
        );
    }
    assert_eq!(
        tv::header_args(
            "fs_search",
            &json!({"query": "x", "glob": "src/**"}),
            &context
        ),
        "\"x\" src/**"
    );
    assert_eq!(
        tv::header_args("glob", &json!({"pattern": "/Users/bob/**"}), &context),
        "\"/Users/<user>/**\""
    );
    assert_eq!(
        tv::header_args(
            "fs_search",
            &json!({"query": "x", "include": "C:/Users/alice/**"}),
            &context
        ),
        "\"x\" ~/**"
    );
    assert_eq!(
        tv::header_args("glob", &json!({"pattern": "/w/src/**"}), &context),
        "\"src/**\""
    );
    assert_eq!(
        tv::header_args(
            "fs_search",
            &json!({"query": "x", "glob": r"C:\Users\bob\**"}),
            &context
        ),
        "\"x\" C:/Users/<user>/**"
    );
    assert_eq!(
        tv::header_args(
            "fs_search",
            &json!({"query": "x", "include": r"\\srv\share\Users\bob\**"}),
            &context
        ),
        "\"x\" //srv/share/Users/<user>/**"
    );
    assert_eq!(
        tv::header_args(
            "fs_search",
            &json!({"query": "x", "include": "../src/**"}),
            &context
        ),
        "\"x\" ../src/**"
    );
    assert_eq!(
        tv::header_args("custom_tool", &json!({"cwd": "/Users/bob/repo"}), &context),
        "/Users/<user>/repo"
    );
}

#[cfg(unix)]
#[test]
fn escaped_absolute_glob_roots_share_the_account_and_home_projection() {
    let context = PathContext {
        workspace: Some("/opt/work".into()),
        homes: vec!["/Users/alice".into()],
    };
    for (pattern, expected) in [
        (r"/U\sers/bob/**", "/Users/<user>/**"),
        (r"\/Users/bob/**", "/Users/<user>/**"),
        (r"\/U\sers/bob/**", "/Users/<user>/**"),
        (r#"'\/Users/o\'brien/**'"#, "'/Users/<user>/**'"),
        (r"/h\ome/carol/**", "/home/<user>/**"),
        (r"/Users/b\ob/**", "/Users/<user>/**"),
        (r"/U\sers/al\ice/a ~b/**", "~/a ~b/**"),
        (r"\/U\sers/al\ice/a ~b/**", "~/a ~b/**"),
        (
            r"{/U\sers/bob/**,/h\ome/carol/**}",
            "{/Users/<user>/**,/home/<user>/**}",
        ),
        (r"**/U\sers/bob/**", r"**/U\sers/bob/**"),
        (r"/opt/u\sers/bob/**", r"/opt/u\u{005c}sers/bob/**"),
        (r"/U\sers/bob/f\?o/**", r"/Users/<user>/f\?o/**"),
        (r"/U\sers/bob/café\?名/**", r"/Users/<user>/café\?名/**"),
        (
            r"/Volumes/Café\, 2024/U\sers/bob/**",
            r"/Volumes/Café\, 2024/Users/<user>/**",
        ),
        (
            r#"/Volumes/Say "hi"/Users/o\'brien/**"#,
            r#"/Volumes/Say "hi"/Users/<user>/**"#,
        ),
        (r"/U\sers/al\ice/Bob\'s notes/**", r"~/Bob\'s notes/**"),
        (r"/U\sers/al\ice", "~"),
        (r"C:\Users\bob\**", "C:/Users/<user>/**"),
    ] {
        let args = json!({"pattern":pattern});
        assert_eq!(
            tv::header_args("glob", &args, &context),
            format!("\"{expected}\""),
            "{pattern}"
        );
        assert_eq!(
            args["pattern"], pattern,
            "raw tool argument must remain unchanged"
        );
    }
}

#[cfg(unix)]
#[test]
fn workspace_alias_to_multiuser_root_keeps_the_account_mask() {
    use std::os::unix::fs::symlink;
    let ancestor = if cfg!(target_os = "macos") {
        "/Users"
    } else {
        "/home"
    };
    let temp = tempfile::tempdir().expect("temporary workspace alias");
    let alias = temp.path().join("account-roots");
    symlink(ancestor, &alias).expect("alias");
    let context = PathContext {
        workspace: Some(alias.display().to_string()),
        homes: vec!["C:/Users/alice".into()],
    };
    let path = alias.join("bob/private");
    assert_eq!(
        context.shorten(&path.to_string_lossy()),
        format!("{ancestor}/<user>/private")
    );
}

#[cfg(unix)]
#[test]
fn interior_workspace_alias_to_accounts_keeps_the_mask_in_all_headers() {
    use std::os::unix::fs::symlink;
    let ancestor = if cfg!(target_os = "macos") {
        "/Users"
    } else {
        "/home"
    };
    let temp = tempfile::tempdir().expect("workspace");
    let alias = temp.path().join("accounts");
    symlink(ancestor, &alias).expect("account alias");
    let paths = [
        alias.join("display-fixture-other-account/missing/file.rs"),
        alias
            .join("..")
            .join(ancestor.trim_start_matches('/'))
            .join("display-fixture-other-account/missing/file.rs"),
    ];
    for homes in [vec![], vec![temp.path().to_path_buf()]] {
        let context = PathContext {
            workspace: Some(temp.path().display().to_string()),
            homes,
        };
        let expected = format!("{ancestor}/<user>/missing/file.rs");
        for path in &paths {
            assert_eq!(context.shorten(&path.to_string_lossy()), expected);
            assert_eq!(
                tv::header_args("fs_read", &json!({"path":path}), &context),
                expected
            );
            assert_eq!(
                tv::header_args("fs_glob", &json!({"pattern":path}), &context),
                format!("\"{expected}\"")
            );
        }
    }
    // An alias that stays inside the workspace keeps the requested readable spelling.
    let real = temp.path().join("real");
    std::fs::create_dir(&real).expect("real directory");
    let internal = temp.path().join("internal");
    symlink(&real, &internal).expect("internal alias");
    let context = PathContext {
        workspace: Some(temp.path().display().to_string()),
        homes: vec![],
    };
    assert_eq!(
        context.shorten(&internal.join("file.rs").to_string_lossy()),
        "internal/file.rs"
    );
}

#[test]
fn a_path_context_change_changes_its_fingerprint() {
    let mut other = paths();
    assert_eq!(paths().fingerprint(), other.fingerprint());
    other.workspace = Some("/elsewhere".to_owned());
    assert_ne!(paths().fingerprint(), other.fingerprint());
}

#[test]
fn commands_and_heredocs_collapse_to_one_line() {
    let heredoc = "cat >> experience-log.jsonl <<'EOF'\n{\"t\":\"2026-09-23T20:01:00Z\",\"tool\":\"fs_write\"}\nEOF";
    assert_eq!(
        tv::one_line(heredoc),
        "cat >> experience-log.jsonl <<'EOF' …",
        "the heredoc body never reaches the header"
    );
    assert_eq!(tv::one_line("  cargo   test\t-p  x  "), "cargo test -p x");
    assert_eq!(
        tv::one_line("\n\n  ls\n\n"),
        "ls",
        "blank lines are not 'more'"
    );
    assert_eq!(tv::one_line(""), "");
}

#[test]
fn header_arguments_are_short_and_single_line() {
    let paths = paths();
    assert_eq!(
        tv::header_args(
            "fs_write",
            &json!({"path": "C:/Users/alice/dev/haider/state/973-LANES.md", "content": "x"}),
            &paths
        ),
        "state/973-LANES.md"
    );
    assert_eq!(
        tv::header_args(
            "edit",
            &json!({"file_path": "C:/Users/alice/notes.md", "old_string": "a", "new_string": "b"}),
            &paths
        ),
        "~/notes.md"
    );
    assert_eq!(
        tv::header_args(
            "process_exec",
            &json!({"command": "cat >> log.jsonl <<'EOF'\n{}\nEOF"}),
            &paths
        ),
        "cat >> log.jsonl <<'EOF' …"
    );
    assert_eq!(
        tv::header_args("process_exec", &json!({"argv": ["git", "status"]}), &paths),
        "git status"
    );
    assert_eq!(
        tv::header_args(
            "fs_search",
            &json!({"pattern": "todos_collapsed", "path": "C:/Users/alice/dev/haider/src"}),
            &paths
        ),
        "\"todos_collapsed\" in src"
    );
    assert_eq!(
        tv::header_args("grep", &json!({"query": "x", "glob": "*.rs"}), &paths),
        "\"x\" *.rs"
    );
    assert_eq!(
        tv::header_args(
            "web_fetch",
            &json!({"url": "https://example.test/a"}),
            &paths
        ),
        "https://example.test/a"
    );
    assert_eq!(
        tv::header_args(
            "Monitor",
            &json!({"desc": "Astra verify\n(re-armed)"}),
            &paths
        ),
        "Astra verify …",
        "the generic summary is one-lined too"
    );
    assert_eq!(tv::header_args("fs_write", &json!({}), &paths), "");
}

#[test]
fn full_arguments_keep_everything_the_header_cut() {
    let heredoc = "cat >> log.jsonl <<'EOF'\n{\"a\":1}\nEOF";
    assert_eq!(
        tv::full_args("process_exec", &json!({"command": heredoc})).as_deref(),
        Some(heredoc)
    );
    let pretty = tv::full_args("web_fetch", &json!({"url": "https://x.test"})).unwrap();
    assert!(pretty.contains("\"url\": \"https://x.test\""), "{pretty}");
    assert_eq!(tv::full_args("todo_write", &json!({})), None);
    // A write's content is its diff: the arguments keep only the rest.
    let write = tv::full_args("fs_write", &json!({"path": "a.md", "content": "BODY"})).unwrap();
    assert!(
        write.contains("\"path\": \"a.md\"") && !write.contains("BODY"),
        "{write}"
    );
    let edit = tv::full_args(
        "edit",
        &json!({"file_path": "a", "old_string": "OLD", "new_string": "NEW"}),
    )
    .unwrap();
    assert!(!edit.contains("OLD") && !edit.contains("NEW"), "{edit}");
    // …but a malformed call with no diff keeps every byte it sent.
    assert!(
        tv::full_args("fs_write", &json!({"content": 3}))
            .unwrap()
            .contains('3')
    );
}

// ---- 2. the diff model --------------------------------------------------

#[test]
fn a_write_is_every_line_added_and_numbered_from_one() {
    let diff = tv::tool_diff(
        "fs_write",
        &json!({"path": "a", "content": "one\ntwo\nthree\n"}),
        &[],
    )
    .expect("a write carries its content");
    assert!(diff.wrote);
    assert_eq!((diff.added, diff.removed), (3, 0));
    assert!(diff.lines.iter().all(|line| line.kind == DiffKind::Added));
    assert_eq!(
        diff.lines
            .iter()
            .map(|line| line.number)
            .collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3)]
    );
}

#[test]
fn an_edit_diffs_its_replacement_line_by_line() {
    let diff = tv::tool_diff(
        "fs_edit",
        &json!({"path": "a.rs", "edits": [{"old": "keep\nold line\ntail", "new": "keep\nnew line\nextra\ntail"}]}),
        &[],
    )
    .expect("an edit carries its replacement");
    let shape: Vec<(DiffKind, &str)> = diff
        .lines
        .iter()
        .map(|line| (line.kind, line.text.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (DiffKind::Context, "keep"),
            (DiffKind::Removed, "old line"),
            (DiffKind::Added, "new line"),
            (DiffKind::Added, "extra"),
            (DiffKind::Context, "tail"),
        ]
    );
    assert_eq!((diff.added, diff.removed), (2, 1));
    assert!(!diff.wrote);
    assert!(
        diff.lines.iter().all(|line| line.number.is_none()),
        "an unresolved edit carries NO numbers rather than invented ones"
    );
    assert_eq!(diff.gutter(), 0);
}

fn span(edit_index: u32, old: Option<u32>, new: Option<u32>) -> EditSpanV1 {
    EditSpanV1 {
        edit_index,
        occurrence: 0,
        old_start_line: old,
        old_line_count: 0,
        new_start_line: new,
        new_line_count: 0,
    }
}

#[test]
fn an_edit_is_numbered_from_the_tools_spans() {
    let diff = tv::tool_diff(
        "edit",
        &json!({"file_path": "a.rs", "old_string": "a\nb", "new_string": "a\nB"}),
        &[span(0, Some(118), Some(118))],
    )
    .unwrap();
    let numbered: Vec<(DiffKind, Option<usize>)> = diff
        .lines
        .iter()
        .map(|line| (line.kind, line.number))
        .collect();
    assert_eq!(
        numbered,
        vec![
            (DiffKind::Context, Some(118)),
            (DiffKind::Removed, Some(119)),
            (DiffKind::Added, Some(119)),
        ]
    );
    assert_eq!(diff.gutter(), 3);
}

/// Removed rows count in the PRE-edit file, added rows in the POST-edit
/// file: verify 3's boundary case, `c` line 3 before and line 2 after.
#[test]
fn removed_and_added_rows_number_from_their_own_sides() {
    let diff = tv::tool_diff(
        "fs_edit",
        &json!({"path": "b.txt", "edits": [{"old": "a\n", "new": "a"}, {"old": "c", "new": "C"}]}),
        &[span(0, Some(1), Some(1)), span(1, Some(3), Some(2))],
    )
    .unwrap();
    let rows: Vec<(DiffKind, Option<usize>, &str)> = diff
        .lines
        .iter()
        .map(|line| (line.kind, line.number, line.text.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (DiffKind::Context, Some(1), "a"),
            (DiffKind::Gap, None, ""),
            (DiffKind::Removed, Some(3), "c"),
            (DiffKind::Added, Some(2), "C"),
        ]
    );
}

#[test]
fn several_edits_are_separated_by_a_gap_and_counted_together() {
    let diff = tv::tool_diff(
        "fs_edit",
        &json!({"path": "a", "edits": [{"old": "x", "new": "y"}, {"old": "p", "new": "q\nr"}]}),
        &[span(0, Some(3), Some(3))],
    )
    .unwrap();
    assert_eq!((diff.added, diff.removed), (3, 2));
    assert_eq!(
        diff.lines
            .iter()
            .filter(|line| line.kind == DiffKind::Gap)
            .count(),
        1
    );
    assert_eq!(diff.lines[0].number, Some(3));
    assert_eq!(
        diff.lines.last().unwrap().number,
        None,
        "edit 2 has no span, so no number"
    );
}

/// No spans (an older journal, another tool), a replace_all with several
/// occurrences, or an unproven side: no number is shown.
#[test]
fn without_a_proven_span_nothing_is_numbered() {
    let args = json!({"path": "a", "edits": [{"old": "x", "new": "y", "replace_all": true}]});
    let none = tv::tool_diff("fs_edit", &args, &[]).unwrap();
    assert!(none.lines.iter().all(|line| line.number.is_none()));
    let mut second = span(0, Some(5), Some(5));
    second.occurrence = 1;
    let many = tv::tool_diff("fs_edit", &args, &[span(0, Some(1), Some(1)), second]).unwrap();
    assert!(
        many.lines.iter().all(|line| line.number.is_none()),
        "several occurrences have no single location"
    );
    let one_sided = tv::tool_diff("fs_edit", &args, &[span(0, None, Some(7))]).unwrap();
    let rows: Vec<(DiffKind, Option<usize>)> = one_sided
        .lines
        .iter()
        .map(|line| (line.kind, line.number))
        .collect();
    assert_eq!(
        rows,
        vec![(DiffKind::Removed, None), (DiffKind::Added, Some(7))]
    );
}

#[test]
fn a_huge_replacement_falls_back_without_losing_a_line() {
    let old: String = (0..1_200).map(|n| format!("old {n}\n")).collect();
    let new: String = (0..1_200).map(|n| format!("new {n}\n")).collect();
    let lines = tv::line_diff(&old, &new, None, None);
    assert_eq!(lines.len(), 2_400, "complete, never truncated");
    assert_eq!(lines[0].kind, DiffKind::Removed);
    assert_eq!(lines[2_399].kind, DiffKind::Added);
}

#[test]
fn edit_pairs_read_both_edit_schemas() {
    let fs_edit = tv::edit_pairs(
        &json!({"path": "a", "edits": [{"old": "x", "new": "y"}, {"old": "p", "new": "q"}]}),
    );
    assert_eq!(fs_edit.len(), 2);
    assert_eq!(
        (fs_edit[1].old.as_str(), fs_edit[1].new.as_str()),
        ("p", "q")
    );
    let edit = tv::edit_pairs(&json!({"file_path": "a", "old_string": "x", "new_string": "y"}));
    assert_eq!((edit[0].old.as_str(), edit[0].new.as_str()), ("x", "y"));
}

#[test]
fn the_preview_window_opens_one_context_row_before_the_first_change() {
    let diff = tv::tool_diff(
        "edit",
        &json!({"file_path": "a", "old_string": "1\n2\n3\n4\nold", "new_string": "1\n2\n3\n4\nnew\nmore\nmore2\nmore3"}),
        &[],
    )
    .unwrap();
    let (start, end) = diff.preview_window(tv::DIFF_PREVIEW_ROWS);
    assert_eq!(diff.lines[start].text, "4", "one row of context");
    assert_eq!(end - start, tv::DIFF_PREVIEW_ROWS);
    // A write has no context: it opens at its first line.
    let write = tv::tool_diff("write", &json!({"file_path": "a", "content": "x\ny"}), &[]).unwrap();
    assert_eq!(write.preview_window(4), (0, 2));
}

#[test]
fn clipped_diff_rows_are_one_row_per_line_and_fit_the_width() {
    let long = "x".repeat(200);
    let diff = tv::tool_diff("write", &json!({"file_path": "a", "content": long}), &[]).unwrap();
    let rows = tv::diff_rows_clipped(&diff, 0..1, 80);
    assert_eq!(rows.len(), 1);
    let cells = tv::DETAIL_INDENT + rows[0].gutter.len() + 3 + rows[0].text.chars().count();
    assert!(cells <= 80, "{cells}");
    assert!(rows[0].text.ends_with('…'));
    // …while the full rows WRAP and keep every character.
    let full = tv::diff_rows(&diff, 0..1, 80);
    assert!(full.len() > 1);
    let joined: String = full.iter().map(|row| row.text.as_str()).collect();
    assert_eq!(joined, "x".repeat(200));
    assert!(matches!(full[0].role, RowRole::Diff { first: true, .. }));
    assert!(matches!(full[1].role, RowRole::Diff { first: false, .. }));
}

// ---- 3. the `⎿` result line ---------------------------------------------

#[test]
fn a_write_and_an_edit_summarise_their_line_counts() {
    let write =
        tv::tool_diff("fs_write", &json!({"path": "a", "content": "1\n2\n3"}), &[]).unwrap();
    assert_eq!(
        result_text(&ResultFacts {
            kind: Some(ToolKind::Write),
            diff: Some(&write),
            ..ResultFacts::default()
        })
        .as_deref(),
        Some("    ⎿ Wrote 3 lines")
    );
    let edit = tv::ToolDiff {
        added: 46,
        removed: 74,
        ..tv::ToolDiff::default()
    };
    assert_eq!(
        result_text(&ResultFacts {
            kind: Some(ToolKind::Edit),
            diff: Some(&edit),
            ..ResultFacts::default()
        })
        .as_deref(),
        Some("    ⎿ Added 46 lines, removed 74 lines"),
        "the reference's own sentence"
    );
    let one = tv::ToolDiff {
        added: 1,
        removed: 0,
        ..tv::ToolDiff::default()
    };
    assert_eq!(
        result_text(&ResultFacts {
            kind: Some(ToolKind::Edit),
            diff: Some(&one),
            ..ResultFacts::default()
        })
        .as_deref(),
        Some("    ⎿ Added 1 line")
    );
}

#[test]
fn a_failed_call_leads_with_a_red_phrase_and_one_reason_line() {
    let facts = ResultFacts {
        kind: Some(ToolKind::Write),
        settle: Settle::Failed,
        reason: Some(
            "Invalid tool call — tool call `toolu_1` ended with\nmalformed JSON arguments",
        ),
        ..ResultFacts::default()
    };
    let segments = tv::result_segments(&facts, 0).unwrap();
    assert_eq!(
        segments_text(&segments),
        "    ⎿ Error writing file · Invalid tool call — tool call `toolu_1` ended with …"
    );
    assert!(
        segments
            .iter()
            .filter(|segment| segment.text.contains("Error") || segment.text.contains("Invalid"))
            .all(|segment| segment.tone == Tone::Err)
    );
    for (settle, phrase) in [
        (Settle::Rejected, "Rejected"),
        (Settle::Conflict, "Conflict"),
        (Settle::Unknown, "Outcome unknown"),
    ] {
        let text = result_text(&ResultFacts {
            kind: Some(ToolKind::Shell),
            settle,
            ..ResultFacts::default()
        })
        .unwrap();
        assert!(text.contains(phrase), "{text}");
    }
    assert_eq!(
        tv::failure_phrase(Some(ToolKind::Edit), Settle::Failed),
        "Error editing file"
    );
    assert_eq!(
        tv::failure_phrase(Some(ToolKind::Read), Settle::Failed),
        "Error reading file"
    );
    assert_eq!(tv::failure_phrase(None, Settle::Failed), "Failed");
}

#[test]
fn a_nonzero_exit_code_leads_the_line_and_a_zero_one_is_silent() {
    let facts = ResultFacts {
        kind: Some(ToolKind::Shell),
        exit_code: Some(2),
        output: "error: linking failed\nmore\n",
        ..ResultFacts::default()
    };
    let segments = tv::result_segments(&facts, 0).unwrap();
    assert_eq!(
        segments_text(&segments),
        "    ⎿ Exit code 2 · error: linking failed"
    );
    assert_eq!(segments[1].tone, Tone::Err);
    let ok = result_text(&ResultFacts {
        kind: Some(ToolKind::Shell),
        exit_code: Some(0),
        output: "1 line\n",
        ..ResultFacts::default()
    })
    .unwrap();
    assert!(!ok.contains("Exit"), "{ok}");
}

#[test]
fn successes_summarise_by_kind() {
    let read = result_text(&ResultFacts {
        kind: Some(ToolKind::Read),
        output: "1\n2\n3\n",
        ..ResultFacts::default()
    });
    assert_eq!(read.as_deref(), Some("    ⎿ Read 3 lines"));
    let search = result_text(&ResultFacts {
        kind: Some(ToolKind::Search),
        matches: Some(1),
        output: "a.rs:1: x\n",
        ..ResultFacts::default()
    });
    assert_eq!(search.as_deref(), Some("    ⎿ Found 1 match"));
    let silent = result_text(&ResultFacts {
        kind: Some(ToolKind::Shell),
        ..ResultFacts::default()
    });
    assert_eq!(silent.as_deref(), Some("    ⎿ (No output)"));
    let shell = result_text(&ResultFacts {
        kind: Some(ToolKind::Shell),
        output: "\nResuming agent a830387\nsecond\nthird\n",
        ..ResultFacts::default()
    });
    assert_eq!(
        shell.as_deref(),
        Some("    ⎿ Resuming agent a830387 … +3 lines")
    );
    let counted = result_text(&ResultFacts {
        kind: Some(ToolKind::Shell),
        output: "one\n",
        output_shown: true,
        ..ResultFacts::default()
    });
    assert_eq!(
        counted.as_deref(),
        Some("    ⎿ 1 line of output"),
        "an expanded row counts rather than quoting its first line twice"
    );
    assert_eq!(
        result_text(&ResultFacts {
            kind: Some(ToolKind::Other),
            ..ResultFacts::default()
        }),
        None,
        "nothing worth a line grows no elbow"
    );
    assert_eq!(
        result_text(&ResultFacts {
            kind: Some(ToolKind::Shell),
            settle: Settle::Running,
            output: "partial\n",
            ..ResultFacts::default()
        }),
        None,
        "a running call speaks on its header"
    );
    assert_eq!(
        result_text(&ResultFacts {
            kind: Some(ToolKind::Shell),
            settle: Settle::Cancelled,
            ..ResultFacts::default()
        })
        .as_deref(),
        Some("    ⎿ Cancelled")
    );
}

#[test]
fn a_cut_tail_never_summarises_with_its_leading_fragment() {
    let text = result_text(&ResultFacts {
        kind: Some(ToolKind::Shell),
        output: "ine 0051 — cut\nline 0052 — whole\n",
        output_cut: true,
        ..ResultFacts::default()
    })
    .unwrap();
    assert!(
        text.contains("line 0052 — whole") && !text.contains("ine 0051"),
        "{text}"
    );
}

#[test]
fn the_result_line_fits_its_row() {
    let facts = ResultFacts {
        kind: Some(ToolKind::Write),
        settle: Settle::Failed,
        reason: Some(&"a very long reason ".repeat(20)),
        ..ResultFacts::default()
    };
    let segments = tv::result_segments(&facts, 80).unwrap();
    let cells: usize = segments
        .iter()
        .map(|segment| unicode_width::UnicodeWidthStr::width(segment.text.as_str()))
        .sum();
    assert!(cells <= 80, "{cells}");
    assert!(segments_text(&segments).ends_with('…'));
}

#[test]
fn fit_segments_keeps_what_fits_and_ellipsizes_the_crossing_segment() {
    let segments = vec![
        Segment::new("abc", Tone::Meta),
        Segment::new("defgh", Tone::Err),
        Segment::new("ij", Tone::Meta),
    ];
    let fitted = tv::fit_segments(segments.clone(), 6);
    assert_eq!(segments_text(&fitted), "abcde…");
    assert_eq!(fitted[1].tone, Tone::Err);
    assert_eq!(
        tv::fit_segments(segments, 10).len(),
        3,
        "a fit is untouched"
    );
    assert_eq!(
        tv::ellipsize_cells("漢字漢字", 5),
        "漢字…",
        "wide glyphs count twice"
    );
}

#[test]
fn the_expand_door_names_what_it_hides_and_the_key() {
    assert_eq!(
        segments_text(&tv::more_segments(12)),
        "      … +12 lines (⌃O to expand)"
    );
    assert_eq!(
        segments_text(&tv::more_segments(1)),
        "      … +1 line (⌃O to expand)"
    );
}

// ---- 4. folds -------------------------------------------------------------

#[test]
fn runs_fold_by_family_and_writes_never_fold() {
    assert_eq!(
        tv::fold_phrase("read", 3),
        ("Read ".to_owned(), "files".to_owned())
    );
    assert_eq!(
        tv::fold_phrase("read", 1),
        ("Read ".to_owned(), "file".to_owned())
    );
    assert_eq!(
        tv::fold_phrase("search", 2),
        ("Searched for ".to_owned(), "patterns".to_owned())
    );
    assert_eq!(
        tv::fold_phrase("command", 3),
        ("Ran ".to_owned(), "shell commands".to_owned())
    );
    assert_eq!(
        tv::fold_phrase("fetch", 2),
        ("Fetched ".to_owned(), "URLs".to_owned())
    );
    assert_eq!(tv::fold_key("fs_write"), None);
    assert_eq!(tv::fold_key("edit"), None);
    assert_eq!(tv::fold_key("fs_read").as_deref(), Some("read"));
    assert_eq!(tv::fold_key("fs_glob").as_deref(), Some("search"));
    assert_eq!(
        tv::fold_key("process_exec").as_deref(),
        Some("command"),
        "a model shell tool folds with model `$` commands"
    );
    assert_eq!(tv::fold_key("verify").as_deref(), Some("verify"));
}

// ---- 5. display rows: nothing is lost -----------------------------------

#[test]
fn detail_rows_carry_every_argument_diff_and_output_character() {
    let command = format!("cat <<'EOF'\n{}\nEOF", "payload ".repeat(30));
    let diff = tv::tool_diff(
        "fs_edit",
        &json!({"path": "a", "edits": [{"old": "a", "new": "b"}]}),
        &[],
    )
    .unwrap();
    let output: String = (0..40).map(|n| format!("output line {n:03}\n")).collect();
    let rows = tv::detail_rows(
        &DetailFacts {
            args: Some(&command),
            diff: Some(&diff),
            output: &output,
            output_truncated: true,
            reason: Some("the full reason, never shortened"),
            headings: true,
            ..DetailFacts::default()
        },
        60,
    );
    let headings: Vec<&str> = rows
        .iter()
        .filter(|row| row.role == RowRole::Heading)
        .map(|row| row.text.as_str())
        .collect();
    assert_eq!(
        headings,
        vec!["Diff · +1 −1", "Output · 40 lines", "Reason", "Arguments"],
        "a diff-carrying call leads with its diff; the raw arguments close"
    );
    let args: String = rows
        .iter()
        .filter(|row| row.role == RowRole::Args)
        .map(|row| row.text.as_str())
        .collect();
    assert_eq!(
        args,
        command.replace('\n', ""),
        "the whole heredoc is reachable"
    );
    let out: Vec<&str> = rows
        .iter()
        .filter(|row| row.role == RowRole::Output)
        .map(|row| row.text.as_str())
        .collect();
    assert_eq!(out.len(), 40);
    assert_eq!(out[39], "output line 039");
    assert!(rows.iter().any(|row| row.text.contains("bounded tail")));
    assert!(rows.iter().any(|row| row.text.contains("never shortened")));
    for row in &rows {
        let cells = tv::DETAIL_INDENT
            + if row.gutter.is_empty() {
                0
            } else {
                row.gutter.len() + 1
            }
            + if matches!(row.role, RowRole::Diff { .. }) {
                2
            } else {
                0
            }
            + unicode_width::UnicodeWidthStr::width(row.text.as_str());
        assert!(cells <= 60, "{row:?}");
    }
    // In place (no headings) the same body carries no section labels.
    let in_place = tv::detail_rows(
        &DetailFacts {
            output: &output,
            ..DetailFacts::default()
        },
        60,
    );
    assert!(in_place.iter().all(|row| row.role == RowRole::Output));
}

#[test]
fn sanitize_makes_tabs_and_controls_measurable() {
    assert_eq!(tv::sanitize("a\tb\u{7}c"), "a    b c");
}

// ---- 6. contrast ---------------------------------------------------------

fn luminance(rgb: haider_tui::theme::Rgb) -> f64 {
    fn channel(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

fn contrast(a: haider_tui::theme::Rgb, b: haider_tui::theme::Rgb) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// Owner: the new rows keep ≥ 4.5:1 in every theme, light and dark. Every
/// ink a tool row sets TEXT in is pinned on the ground it sits on.
#[test]
fn every_tool_row_text_ink_clears_4_5_on_its_ground_in_every_theme() {
    for key in haider_tui::theme::ThemeKey::ALL {
        let theme = key.theme();
        let floor = |name: &str, ink, ground| {
            let ratio = contrast(ink, ground);
            assert!(ratio >= 4.5, "{}: {name} {ratio:.2} under 4.5", theme.label);
        };
        floor("text on added diff", theme.text, theme.diff_added_ground());
        floor(
            "text on removed diff",
            theme.text,
            theme.diff_removed_ground(),
        );
        floor("verb (maroon)", theme.maroon, theme.bg);
        floor("argument / meta (dim)", theme.dim, theme.bg);
        floor("failure phrase (err)", theme.err, theme.bg);
        floor("body", theme.text, theme.bg);
        floor("count (bright)", theme.bright, theme.bg);
        // EVERY tone, as the tool-row renderer inks it, on the page ground
        // and on the focused row's hover band (973 repair: Desert's accent
        // gold was 4.01:1 on a `file:line` token).
        for tone in [
            Tone::Body,
            Tone::Meta,
            Tone::Structure,
            Tone::Name,
            Tone::Emphasis,
            Tone::Accent,
            Tone::Ok,
            Tone::Warn,
            Tone::Err,
        ] {
            for (ground_name, ground) in [("page", theme.bg), ("hover band", theme.sel_bg)] {
                let ink = theme.legible(theme.tool_tone_ink(tone), ground);
                floor(&format!("{tone:?} on the {ground_name}"), ink, ground);
            }
            // An ink already over the floor is never touched.
            let raw = theme.tool_tone_ink(tone);
            if contrast(raw, theme.bg) >= 4.5 {
                assert_eq!(
                    theme.legible(raw, theme.bg),
                    raw,
                    "{}: {tone:?}",
                    theme.label
                );
            }
        }
        // The two diff grounds must also read APART from the page.
        assert_ne!(theme.diff_added_ground(), theme.bg);
        assert_ne!(theme.diff_removed_ground(), theme.bg);
        assert_ne!(theme.diff_added_ground(), theme.diff_removed_ground());
    }
}

#[test]
fn syntax_prefixed_glob_branches_use_the_shared_projection() {
    use haider_tui::{toolfold::semantic_summary, toolview::project_absolute_pattern};
    for (pattern, expected) in [
        ("!/Users/bob/**", "!/Users/<user>/**"),
        ("!!/Users/bob/**", "!!/Users/<user>/**"),
        (
            "{/Users/bob,/home/carol}/**",
            "{/Users/<user>,/home/<user>}/**",
        ),
        ("{src,/Users/bob}/**", "{src,/Users/<user>}/**"),
        (" /Users/bob/**", " /Users/<user>/**"),
        ("\t/Users/bob/**", "\t/Users/<user>/**"),
        ("~bob/**", "~<user>/**"),
        ("!~BOB/**", "!~<user>/**"),
        ("{~bob,~carol}/**", "{~<user>,~<user>}/**"),
        (r"!C:\Users\bob\**", "!C:/Users/<user>/**"),
        (
            r"{C:\Users\bob,D:\temp}\**",
            r"{C:/Users/<user>,D:/temp}\**",
        ),
        (r"!\\server\Users\bob\**", "!//server/Users/<user>/**"),
        (r"!\\?\C:\Users\bob\**", "!C:/Users/<user>/**"),
        (r"!\\.\C:\Users\bob\**", "!C:/Users/<user>/**"),
        (
            r"!\\?\Volume{abc}\Users\bob\**",
            "!//device/Volume{abc}/Users/<user>/**",
        ),
        (r"!\??\C:\Users\bob\**", "!C:/Users/<user>/**"),
        (
            "{!/Users/bob,{/home/carol,src}}/**",
            "{!/Users/<user>,{/home/<user>,src}}/**",
        ),
        ("**/Users/bob/**", "**/Users/bob/**"),
        ("src/**", "src/**"),
        ("../bob/**", "../bob/**"),
        ("~/**", "~/**"),
        ("!src/**", "!src/**"),
        ("{src,tests}/**", "{src,tests}/**"),
        ("src/~bob/**", "src/~bob/**"),
        ("src!/Users/bob/**", "src!/Users/bob/**"),
        ("src/{/Users/bob,lib}/**", "src/{/Users/bob,lib}/**"),
        ("src,/Users/bob/**", "src,/Users/bob/**"),
        ("/Users/{bob,carol}/**", "/Users/<user>/**"),
        ("!/Users/{bob,carol}/**", "!/Users/<user>/**"),
        ("/Users/bob/{src,tests}/**", "/Users/<user>/{src,tests}/**"),
        (
            "@(/Users/bob|/home/carol)/**",
            "@(/Users/<user>|/home/<user>)/**",
        ),
        (
            "!(/Users/bob|/home/carol)/**",
            "!(/Users/<user>|/home/<user>)/**",
        ),
        (
            "/Users/bob/** /home/carol/**",
            "/Users/<user>/** /home/carol/**",
        ),
        (
            "!C:/Documents and Settings/bob/**",
            "!C:/Documents and Settings/<user>/**",
        ),
    ] {
        assert_eq!(project_absolute_pattern(pattern), expected, "{pattern}");
        for key in ["glob", "include"] {
            assert_eq!(
                haider_tui::toolfold::arg_summary(&json!({key:pattern})),
                expected
            );
        }
        let paths = PathContext::default();
        for (name, args) in [
            ("fs_search", json!({"query":"x", "glob":pattern})),
            ("fs_search", json!({"query":"x", "include":pattern})),
            ("glob", json!({"pattern":pattern})),
            ("fs_glob", json!({"query":pattern})),
        ] {
            let header = tv::header_args(name, &args, &paths);
            let plain = semantic_summary(name, &args);
            // Tool headers intentionally collapse whitespace onto one line.
            assert!(
                header.contains(&haider_tui::toolview::one_line(expected)),
                "{name} {pattern}: {header}"
            );
            assert!(plain.contains(expected), "{name} {pattern}: {plain}");
        }
    }
}

#[test]
fn expanded_broad_workspace_roots_never_restore_the_account() {
    for (workspace, path) in [
        (r"\\srv\share\msys64", r"\\srv\share\msys64\home\bob\x"),
        (r"\\srv\msys64\home", r"\\srv\msys64\home\bob\x"),
        (
            r"\\srv\share\Documents and Settings",
            r"\\srv\share\Documents and Settings\bob\x",
        ),
        (
            r"\\srv\Documents and Settings",
            r"\\srv\Documents and Settings\bob\x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\msys64",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\msys64\home\bob\x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\msys64\home",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\msys64\home\bob\x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Documents and Settings",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Documents and Settings\bob\x",
        ),
        (
            r"\\?\Volume{abc}\cache\alice",
            r"\\?\Volume{abc}\cache\alice\Users\bob\x",
        ),
        ("C:/msys64", "C:/msys64/home/bob/x"),
        ("C:/msys64/home", "C:/msys64/home/bob/x"),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Users\bob\x",
        ),
        (
            r"\\.\GLOBALROOT\Device\HarddiskVolume3\Users",
            r"\\.\GLOBALROOT\Device\HarddiskVolume3\Users\bob\x",
        ),
    ] {
        let paths = PathContext {
            workspace: Some(workspace.into()),
            homes: vec!["C:/Users/alice".into()],
        };
        assert!(
            !paths.shorten(path).to_ascii_lowercase().contains("bob"),
            "{workspace} {path}"
        );
    }
}

fn assert_pattern_routes(pattern: &str, expected: &str, context: &PathContext) {
    use haider_protocol::{
        EventPayload,
        ids::ItemId,
        item::{ItemEvent, ToolStatus, TurnItem},
    };
    use haider_tui::{plain::render_plain_with_context, projection::SessionProjection};
    for (name, args, search) in [
        ("fs_search", json!({"query":"needle", "glob":pattern}), true),
        (
            "fs_search",
            json!({"query":"needle", "include":pattern}),
            true,
        ),
        ("glob", json!({"pattern":pattern}), false),
        ("fs_glob", json!({"query":pattern}), false),
    ] {
        let summary = if search {
            format!("\"needle\" {expected}")
        } else {
            expected.to_owned()
        };
        let compact = tv::one_line(expected);
        let header = if search {
            format!("\"needle\" {compact}")
        } else {
            format!("\"{compact}\"")
        };
        assert_eq!(
            tv::header_args(name, &args, context),
            header,
            "{name} {pattern}"
        );
        let mut projection = SessionProjection::new();
        projection.apply(&EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("pattern-parity"),
            item: TurnItem::ToolCall {
                call_id: "pattern-parity".into(),
                name: name.into(),
                args,
                status: ToolStatus::InProgress,
            },
        }));
        let plain = render_plain_with_context(&projection, 0, None, context);
        assert_eq!(
            plain.lines().next(),
            Some(format!("⚒ {name} ◐ · {summary} · status running").as_str()),
            "{pattern}"
        );
    }
}

#[test]
fn unenclosed_punctuation_and_whitespace_belong_to_the_whole_absolute_pattern() {
    let context = PathContext {
        workspace: Some("/opt/w".into()),
        homes: vec!["/Users/alice".into(), "C:/Users/alice".into()],
    };
    for (pattern, expected) in [
        (
            "/Volumes/Bob's Passport/Users/carol/src/**",
            "/Volumes/Bob's Passport/Users/<user>/src/**",
        ),
        (r"C:\Users\o'brien\src\**", "C:/Users/<user>/src/**"),
        (
            r"\\srv\a,b\Users\carol\src\**",
            "//srv/a,b/Users/<user>/src/**",
        ),
        (
            "/Volumes/Backup, 2024/Users/carol/**",
            "/Volumes/Backup, 2024/Users/<user>/**",
        ),
        (
            r"\\?\Volume{abc}\a,b\Users\carol\**",
            "//device/Volume{abc}/a,b/Users/<user>/**",
        ),
        ("/Users/al}ice/**", "/Users/<user>/**"),
        (r"C:\Users\carol)x\**", "C:/Users/<user>/**"),
        (
            "/Volumes/A|B/Users/carol/**",
            "/Volumes/A|B/Users/<user>/**",
        ),
        (
            r#"/Volumes/Say "hi"/Users/carol/**"#,
            r#"/Volumes/Say "hi"/Users/<user>/**"#,
        ),
        (r"\\.\C:\Users\o'brien\**", "C:/Users/<user>/**"),
        (r"\??\C:\Users\o'brien\**", "C:/Users/<user>/**"),
        (
            r"\\?\UNC\srv\a|b\Users\carol\**",
            "//srv/a|b/Users/<user>/**",
        ),
        (
            r"\\?\GLOBALROOT\Device\Mup\srv\a,b\Users\carol\**",
            "//device/GLOBALROOT/Device/Mup/srv/a,b/Users/<user>/**",
        ),
        (
            "C:/Documents and Settings/o'brien/**",
            "C:/Documents and Settings/<user>/**",
        ),
        ("C:/msys64/home/o'brien/**", "C:/msys64/home/<user>/**"),
        ("C:/Users/alice/a ~b/x/**", "~/a ~b/x/**"),
        ("C:/Users/alice/Bob's notes/**", "~/Bob's notes/**"),
        ("C:/Users/alice/a,b/x/**", "~/a,b/x/**"),
        ("C:/Users/alice/x (1)/**", "~/x (1)/**"),
        ("/opt/w/a,b/x/**", "a,b/x/**"),
        ("/opt/w/src/x (1)/**", "src/x (1)/**"),
        ("C:/Program Files (x86)/**", "C:/Program Files (x86)/**"),
    ] {
        assert_eq!(context.shorten(pattern), expected, "whole path: {pattern}");
        for prefix in ["", "!", "!!", " ", "! "] {
            assert_pattern_routes(
                &format!("{prefix}{pattern}"),
                &format!("{prefix}{expected}"),
                &context,
            );
        }
    }
}

#[test]
fn a_branch_ends_only_at_its_own_enclosing_syntax() {
    let context = PathContext {
        workspace: Some("/opt/w".into()),
        homes: vec!["C:/Users/alice".into()],
    };
    for (pattern, expected) in [
        (
            "{/Volumes/Bob's Passport/Users/carol/**,/tmp/**}",
            "{/Volumes/Bob's Passport/Users/<user>/**,/tmp/**}",
        ),
        ("{/Users/al)ice/**,/tmp/**}", "{/Users/<user>/**,/tmp/**}"),
        (
            "{/Volumes/Backup (old/Users/carol/**,/home/bob/**}",
            "{/Volumes/Backup (old/Users/<user>/**,/home/<user>/**}",
        ),
        (
            "@(/Volumes/Backup (old/Users/carol/**|/home/bob/**)",
            "@(/Volumes/Backup (old/Users/<user>/**|/home/<user>/**)",
        ),
        (
            "{/Users/bob/@(src|lib)/**,/home/carol/**}",
            "{/Users/<user>/@(src|lib)/**,/home/<user>/**}",
        ),
        (
            "@(/Volumes/Backup, 2024/Users/carol/**|/tmp/**)",
            "@(/Volumes/Backup, 2024/Users/<user>/**|/tmp/**)",
        ),
        ("@(/Users/al}ice/**|/tmp/**)", "@(/Users/<user>/**|/tmp/**)"),
        (
            "\"/Volumes/Bob's Passport/Users/carol/**\"",
            "\"/Volumes/Bob's Passport/Users/<user>/**\"",
        ),
        (
            "'/Volumes/Backup, 2024/Users/carol/**'",
            "'/Volumes/Backup, 2024/Users/<user>/**'",
        ),
        (
            "{C:/Users/alice/a ~b/x/**,/tmp/**}",
            "{~/a ~b/x/**,/tmp/**}",
        ),
        (
            "{!/Users/bob,{/home/carol,src}}/**",
            "{!/Users/<user>,{/home/<user>,src}}/**",
        ),
        (
            "@({/Users/bob,/home/carol}|/tmp)/**",
            "@({/Users/<user>,/home/<user>}|/tmp)/**",
        ),
        (
            "{/Users/{bob,carol}/**,/tmp/**}",
            "{/Users/<user>/**,/tmp/**}",
        ),
        ("src/{/Users/bob,lib}/**", "src/{/Users/bob,lib}/**"),
        ("src/~bob/**", "src/~bob/**"),
        (
            "'/Users/bob/**' /home/carol/**",
            "'/Users/<user>/**' /home/<user>/**",
        ),
        ("'/Users/bob/**' ~carol/**", "'/Users/<user>/**' ~<user>/**"),
        (
            "{/Users/bob,/tmp} /home/carol/**",
            "{/Users/<user>,/tmp} /home/<user>/**",
        ),
        (
            "@(/Users/bob|/tmp) /home/carol/**",
            "@(/Users/<user>|/tmp) /home/<user>/**",
        ),
        ("src,/Users/bob/**", "src,/Users/bob/**"),
        ("/opt/w/a ~bob/x/**", "a ~bob/x/**"),
    ] {
        assert_pattern_routes(pattern, expected, &context);
    }
}

#[test]
fn posix_pattern_routes_match_the_whole_path_projection() {
    let context = PathContext {
        workspace: Some("/opt/w".into()),
        homes: vec!["/Users/alice".into()],
    };
    for tail in [
        "a ~b/x/**",
        "Bob's notes/**",
        "Library/Application Support/Code/**",
        "Developer/my app (v2)/**/*.{rs,toml}",
    ] {
        let pattern = format!("/Users/alice/{tail}");
        // Whether this platform accepts a POSIX home is a separate policy.
        // Every route must keep the whole-path label and its literal tail.
        let expected = context.shorten(&pattern);
        assert!(expected.ends_with(tail), "{expected}");
        assert_pattern_routes(&pattern, &expected, &context);
    }
}

#[cfg(windows)]
#[test]
fn windows_rooted_patterns_use_the_whole_account_projection() {
    let context = PathContext {
        workspace: Some("C:/repo".into()),
        homes: vec!["C:/Users/alice".into()],
    };
    for pattern in [
        r"\Users\o'brien\**",
        r"\Users\carol)x\**",
        r"\Users\al}ice\**",
        r#"\Users\say "hi"\**"#,
    ] {
        assert_eq!(context.shorten(pattern), "/Users/<user>/**");
        assert_pattern_routes(pattern, "/Users/<user>/**", &context);
    }
    assert_pattern_routes(r"src\Users\bob\**", r"src\Users\bob\**", &context);
}

#[cfg(unix)]
#[test]
fn escaped_posix_delimiters_do_not_close_a_pattern_branch() {
    let context = PathContext {
        workspace: None,
        homes: vec!["C:/Users/alice".into()],
    };
    for (pattern, expected) in [
        (r"'/Users/o\'brien/**'", "'/Users/<user>/**'"),
        (r"'~o\'brien/**'", "'~<user>/**'"),
        (
            r#""/Volumes/Say \"hi\"/Users/carol/**""#,
            r#""/Volumes/Say \"hi\"/Users/<user>/**""#,
        ),
        (
            r"{/Volumes/Backup\, 2024/Users/carol/**,/tmp/**}",
            r"{/Volumes/Backup\, 2024/Users/<user>/**,/tmp/**}",
        ),
        (
            r"@(/Volumes/A\|B/Users/carol/**|/tmp/**)",
            r"@(/Volumes/A\|B/Users/<user>/**|/tmp/**)",
        ),
    ] {
        assert_pattern_routes(pattern, expected, &context);
    }
    assert_pattern_routes(r"'C:\Users\bob\'", "'C:/Users/<user>'", &context);
}
