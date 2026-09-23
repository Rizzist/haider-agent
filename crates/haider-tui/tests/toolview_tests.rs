//! 973-tui-toolview — the PURE half of the Claude-Code-style tool rows:
//! verbs, path shortening, one-line commands, the diff model, the `⎿`
//! result line, fold phrases, display rows and the diff/text contrast floor.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_tui::toolfold::{Segment, Tone, segments_text};
use haider_tui::toolview::{
    self as tv, DetailFacts, DiffKind, PathContext, ResultFacts, RowRole, Settle, ToolKind,
};
use serde_json::json;

fn paths() -> PathContext {
    PathContext {
        workspace: Some("/Users/alice/dev/haider".to_owned()),
        home: Some("/Users/alice".to_owned()),
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
        paths.shorten("/Users/alice/dev/haider/state/973-LANES.md"),
        "state/973-LANES.md"
    );
    assert_eq!(paths.shorten("/Users/alice/dev/haider"), ".");
    assert_eq!(
        paths.shorten("/Users/alice/Developer/notes.md"),
        "~/Developer/notes.md"
    );
    assert_eq!(paths.shorten("/Users/alice"), "~");
    // Component-aware: a sibling that merely shares a prefix is NOT inside.
    assert_eq!(paths.shorten("/Users/alice2/x.rs"), "/Users/alice2/x.rs");
    assert_eq!(
        paths.shorten("/Users/alice/dev/haider-old/x.rs"),
        "~/dev/haider-old/x.rs"
    );
    assert_eq!(paths.shorten("/etc/hosts"), "/etc/hosts");
    assert_eq!(paths.shorten("src/app.rs"), "src/app.rs", "relative stays");
    // A trailing slash on the root changes nothing.
    let slashed = PathContext {
        workspace: Some("/w/".to_owned()),
        home: None,
    };
    assert_eq!(slashed.shorten("/w/a.rs"), "a.rs");
    // No context shortens nothing.
    assert_eq!(PathContext::default().shorten("/w/a.rs"), "/w/a.rs");
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
            &json!({"path": "/Users/alice/dev/haider/state/973-LANES.md", "content": "x"}),
            &paths
        ),
        "state/973-LANES.md"
    );
    assert_eq!(
        tv::header_args(
            "edit",
            &json!({"file_path": "/Users/alice/notes.md", "old_string": "a", "new_string": "b"}),
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
            &json!({"pattern": "todos_collapsed", "path": "/Users/alice/dev/haider/src"}),
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

#[test]
fn a_resolved_anchor_numbers_the_edit_in_file_lines() {
    let diff = tv::tool_diff(
        "edit",
        &json!({"file_path": "a.rs", "old_string": "a\nb", "new_string": "a\nB"}),
        &[Some(118)],
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

#[test]
fn several_edits_are_separated_by_a_gap_and_counted_together() {
    let diff = tv::tool_diff(
        "fs_edit",
        &json!({"path": "a", "edits": [{"old": "x", "new": "y"}, {"old": "p", "new": "q\nr"}]}),
        &[Some(3), None],
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
        "edit 2 was unresolved"
    );
}

#[test]
fn a_huge_replacement_falls_back_without_losing_a_line() {
    let old: String = (0..1_200).map(|n| format!("old {n}\n")).collect();
    let new: String = (0..1_200).map(|n| format!("new {n}\n")).collect();
    let lines = tv::line_diff(&old, &new, None);
    assert_eq!(lines.len(), 2_400, "complete, never truncated");
    assert_eq!(lines[0].kind, DiffKind::Removed);
    assert_eq!(lines[2_399].kind, DiffKind::Added);
}

#[test]
fn line_of_finds_the_first_occurrence() {
    assert_eq!(tv::line_of("a\nb\nc\nb", "b"), Some(2));
    assert_eq!(tv::line_of("a\nb", "a"), Some(1));
    assert_eq!(tv::line_of("a\nb", "zz"), None);
    assert_eq!(
        tv::line_of("a", ""),
        None,
        "an empty replacement has no place"
    );
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
        // The two diff grounds must also read APART from the page.
        assert_ne!(theme.diff_added_ground(), theme.bg);
        assert_ne!(theme.diff_removed_ground(), theme.bg);
        assert_ne!(theme.diff_added_ground(), theme.diff_removed_ground());
    }
}
