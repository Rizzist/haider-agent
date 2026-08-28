#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{ArtifactRef, SessionId};
use haider_protocol::tool::{ToolResultData, ToolTruncationReason};
use haider_tools::{
    CasSink, EffectBroker, FsCaseMode, FsFileGlob, FsGlob, FsRead, FsSearch, FsSearchMode,
    JournalSink, PermissionPolicy, ResultBounds, SEARCH_MAX_LINE_BYTES, SEARCH_PATTERN_MAX_BYTES,
    SEARCH_REGEX_PATTERN_MAX_BYTES, ToolError, ToolResult,
};
use std::fs;
use std::path::Path;

#[derive(Default)]
struct TestJournal;

#[async_trait::async_trait]
impl JournalSink for TestJournal {
    async fn append(&mut self, _payload: EventPayload) -> ToolResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingCas(Vec<Vec<u8>>);

#[async_trait::async_trait]
impl CasSink for RecordingCas {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        self.0.push(bytes.to_vec());
        Ok(ArtifactRef::new(format!(
            "blake3:{}",
            blake3::hash(bytes).to_hex()
        )))
    }

    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
        let bytes = fs::read(path).map_err(|error| ToolError::cas(error.to_string()))?;
        self.put(&bytes).await
    }
}

fn broker(root: &Path, generation: u64) -> EffectBroker {
    EffectBroker::new_at(
        Box::new(TestJournal),
        root,
        SessionId::new(format!("search-{generation}")),
        generation,
        1_800_000_000_000 + generation,
    )
    .expect("broker")
}

fn allow_read() -> PermissionPolicy {
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsRead);
    policy
}

/// MUTATION CHECK: remove a RegexBuilder limit, context bound, column mapping,
/// or file-glob branch. Expected failure: rejection or structured fields drift.
#[tokio::test]
async fn regex_search_is_limited_structured_contextual_and_file_filtered() {
    let root = tempfile::tempdir().expect("root");
    fs::create_dir(root.path().join(".git")).expect("git marker");
    fs::write(
        root.path().join("keep.rs"),
        "before\nNeedle needle\nafter\n",
    )
    .expect("keep");
    fs::write(root.path().join("drop.txt"), "Needle\n").expect("drop");
    let mut broker = broker(root.path(), 1);
    let result = broker
        .fs_search(
            &FsSearch::new(".", r"n(e+)dle")
                .with_mode(FsSearchMode::Regex)
                .with_case_mode(FsCaseMode::Insensitive)
                .with_context(1, 1)
                .with_max_matches(5)
                .with_file_glob(FsFileGlob::new(vec!["*.rs".into()], vec![])),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("regex search");

    assert_eq!(result.preview, "keep.rs:2:Needle needle\n");
    let Some(ToolResultData::FsSearch {
        matches,
        truncated_reason,
        files_scanned,
        ..
    }) = result.data
    else {
        panic!("typed search data");
    };
    assert_eq!(truncated_reason, None);
    assert_eq!(files_scanned, 1);
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].column, 1);
    assert_eq!(matches[1].column, 8);
    assert_eq!(matches[0].context_before, vec!["before"]);
    assert_eq!(matches[0].context_after, vec!["after"]);

    fs::write(root.path().join("anchors.rs"), "zero\nNeedle\n").expect("anchors");
    let anchored = broker
        .fs_search(
            &FsSearch::new(".", "^Needle$")
                .with_mode(FsSearchMode::Regex)
                .with_file_glob(FsFileGlob::new(vec!["anchors.rs".into()], vec![])),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("non-multiline anchors");
    assert!(anchored.preview.is_empty());
    let multiline = broker
        .fs_search(
            &FsSearch::new(".", "^Needle$")
                .with_mode(FsSearchMode::Regex)
                .with_multiline(true)
                .with_file_glob(FsFileGlob::new(vec!["anchors.rs".into()], vec![])),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("multiline anchors");
    let Some(ToolResultData::FsSearch { matches, .. }) = multiline.data else {
        panic!("typed multiline search");
    };
    assert_eq!(matches[0].line, 2);
    assert!(matches[0].context_after.is_empty());

    fs::write(root.path().join("greedy.rs"), "foo\nbar\n").expect("greedy fixture");
    let greedy = broker
        .fs_search(
            &FsSearch::new(".", "foo.*")
                .with_mode(FsSearchMode::Regex)
                .with_file_glob(FsFileGlob::new(vec!["greedy.rs".into()], vec![])),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("non-multiline greedy regex");
    assert!(greedy.preview.contains("greedy.rs:1:foo"));

    fs::write(root.path().join("case.rs"), "abc\nABC\n").expect("case fixture");
    let inline_case = broker
        .fs_search(
            &FsSearch::new(".", "(?-i:ABC)")
                .with_mode(FsSearchMode::Regex)
                .with_case_mode(FsCaseMode::Insensitive)
                .with_file_glob(FsFileGlob::new(vec!["case.rs".into()], vec![])),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("inline case override");
    let Some(ToolResultData::FsSearch { matches, .. }) = inline_case.data else {
        panic!("typed inline-case search");
    };
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line, 2);

    let nested = format!("{}x{}", "(".repeat(130), ")".repeat(130));
    let error = broker
        .fs_search(
            &FsSearch::new(".", nested).with_mode(FsSearchMode::Regex),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect_err("nest limit");
    assert!(
        matches!(error, ToolError::InvalidArgument { ref message } if message.contains("invalid fs_search regex"))
    );
    let nfa_error = broker
        .fs_search(
            &FsSearch::new(".", "[A-Za-z]{100000}").with_mode(FsSearchMode::Regex),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect_err("regex NFA size limit");
    assert!(
        matches!(nfa_error, ToolError::InvalidArgument { ref message } if message.contains("invalid fs_search regex"))
    );
    let source_error = broker
        .fs_search(
            &FsSearch::new(".", "x".repeat(SEARCH_PATTERN_MAX_BYTES + 1))
                .with_mode(FsSearchMode::Regex),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect_err("regex source limit");
    assert!(matches!(source_error, ToolError::InvalidArgument { .. }));
    let regex_source_error = broker
        .fs_search(
            &FsSearch::new(".", r"\w".repeat(SEARCH_REGEX_PATTERN_MAX_BYTES / 2 + 1))
                .with_mode(FsSearchMode::Regex),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect_err("pre-HIR regex source limit");
    assert!(matches!(
        regex_source_error,
        ToolError::InvalidArgument { .. }
    ));
}

#[test]
fn regex_dfa_limit_is_wired_into_the_builder() {
    let source = include_str!("../src/filesystem.rs");
    assert_eq!(
        source
            .matches(".dfa_size_limit(SEARCH_REGEX_DFA_SIZE_LIMIT)")
            .count(),
        1,
        "MUTATION CHECK: the documented DFA cache limit must reach RegexBuilder"
    );
}

#[tokio::test]
async fn overlong_lines_are_reported_instead_of_silently_omitted() {
    let root = tempfile::tempdir().expect("root");
    let mut line = "a".repeat(SEARCH_MAX_LINE_BYTES + 1);
    line.push_str("needle\n");
    fs::write(root.path().join("long.txt"), line).expect("long line");
    let mut broker = broker(root.path(), 11);
    let result = broker
        .fs_search(
            &FsSearch::new(".", "needle"),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("bounded search");
    let Some(ToolResultData::FsSearch {
        truncated_reason, ..
    }) = result.data
    else {
        panic!("typed search data");
    };
    assert_eq!(truncated_reason, Some(ToolTruncationReason::LineTooLong));
    assert!(result.preview.is_empty());
}

/// MUTATION CHECK: expose a denied path or raw token in either the legacy
/// preview or structured match, or spill a small redacted result to CAS.
/// Expected failure: a literal secret is visible or the CAS remains nonempty.
#[tokio::test]
async fn search_and_read_redact_without_spilling_inline_results() {
    let root = tempfile::tempdir().expect("root");
    fs::create_dir(root.path().join(".git")).expect("git marker");
    fs::write(root.path().join(".env"), "PASSWORD=never-show\n").expect("env");
    fs::write(
        root.path().join(".npmrc"),
        format!("{}\n_authToken=late-secret\n", "# filler\n".repeat(1_100)),
    )
    .expect("late token config");
    fs::write(root.path().join("binary.txt"), b"needle\0secret").expect("binary");
    let token = "sk-abcdefghijklmnopQRSTUV";
    fs::write(root.path().join("normal.txt"), format!("token={token}\n")).expect("normal");
    let three_kib = "plain source line without adapter noise\n".repeat(80);
    fs::write(root.path().join("three-kib.txt"), &three_kib).expect("3 KiB fixture");
    fs::write(
        root.path().join("embedded-key.txt"),
        "-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n",
    )
    .expect("embedded key");
    let mut broker = broker(root.path(), 2);
    let mut cas = RecordingCas::default();

    let result = broker
        .fs_search(
            &FsSearch::new(".", "sk-").with_repo_options(true, true),
            &allow_read(),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("search");
    assert!(!result.preview.contains(token));
    assert!(result.preview.contains("[REDACTED:api_key]"));
    assert!(result.artifact.is_none());
    let Some(ToolResultData::FsSearch {
        matches,
        binary_files_skipped,
        skipped_sensitive,
        bytes_scanned,
        ..
    }) = result.data
    else {
        panic!("typed search data");
    };
    assert_eq!(binary_files_skipped, 1);
    assert!(bytes_scanned >= b"needle\0secret".len());
    assert_eq!(skipped_sensitive, 2);
    assert!(!matches[0].text.contains(token));
    assert!(cas.0.is_empty());

    let read = broker
        .fs_read(
            &FsRead::new("normal.txt"),
            &allow_read(),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("read");
    assert!(!read.preview.contains(token));
    assert!(read.artifact.is_none());
    assert!(cas.0.is_empty());

    let env = broker
        .fs_read(
            &FsRead::new(".env"),
            &allow_read(),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("sensitive read");
    assert_eq!(env.preview, "[REDACTED:sensitive_file]\n");
    assert!(env.artifact.is_none());
    assert!(cas.0.is_empty());

    let plain = broker
        .fs_read(
            &FsRead::new("three-kib.txt"),
            &allow_read(),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("3 KiB read");
    assert_eq!(plain.preview, three_kib);
    assert!(plain.artifact.is_none());

    let ranged = broker
        .fs_read(
            &FsRead::new("embedded-key.txt").with_line_range(Some(2), Some(1)),
            &allow_read(),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("ranged key-body read");
    assert_eq!(ranged.preview, "2: [REDACTED:private_key]\n");
    assert!(ranged.artifact.is_none());
    assert!(cas.0.is_empty());

    let searched_key = broker
        .fs_search(
            &FsSearch::new(".", "AA==").with_repo_options(false, true),
            &allow_read(),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("search embedded key body");
    assert!(searched_key.preview.contains("[REDACTED:private_key]"));
    assert!(!searched_key.preview.contains("AA=="));
    assert!(searched_key.artifact.is_none());
    assert!(cas.0.is_empty());
}

#[tokio::test]
async fn repository_ignore_hidden_and_sensitive_glob_policies_are_stable() {
    let root = tempfile::tempdir().expect("root");
    fs::create_dir_all(root.path().join(".git/info")).expect("git");
    fs::create_dir(root.path().join("src")).expect("src");
    fs::write(root.path().join(".gitignore"), "ignored.rs\n").expect("ignore");
    fs::write(root.path().join("src/.gitignore"), "drop.rs\n").expect("nested ignore");
    fs::write(root.path().join("ignored.rs"), "x").expect("ignored");
    fs::write(root.path().join("src/drop.rs"), "x").expect("drop");
    fs::write(root.path().join("src/keep.rs"), "x").expect("keep");
    fs::write(root.path().join("private.pem"), "x").expect("pem");
    fs::write(root.path().join(".hidden.rs"), "x").expect("hidden");
    let mut broker = broker(root.path(), 3);

    let result = broker
        .fs_glob(
            &FsGlob::new(".", "**/*.rs").with_repo_options(true, true),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("glob");
    assert_eq!(result.preview, ".hidden.rs\nsrc/keep.rs\n");
    let Some(ToolResultData::FsGlob {
        truncated_reason,
        skipped_sensitive,
        ..
    }) = result.data
    else {
        panic!("typed glob data");
    };
    assert_eq!(truncated_reason, None);
    assert_eq!(skipped_sensitive, 0, "nonmatching PEM is never exposed");

    let sensitive = broker
        .fs_glob(
            &FsGlob::new(".", "**/*").with_repo_options(false, true),
            &allow_read(),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("sensitive glob");
    assert!(!sensitive.preview.contains("private.pem"));
    let Some(ToolResultData::FsGlob {
        skipped_sensitive, ..
    }) = sensitive.data
    else {
        panic!("typed glob data");
    };
    assert_eq!(skipped_sensitive, 1);
}

#[tokio::test]
async fn directory_read_collapses_extensions_and_reports_its_entry_cap() {
    let root = tempfile::tempdir().expect("root");
    for index in 0..510 {
        fs::write(root.path().join(format!("file-{index:03}.rs")), "x").expect("entry");
    }
    fs::create_dir(root.path().join("node_modules")).expect("vendor directory");
    let mut broker = broker(root.path(), 4);
    let mut cas = RecordingCas::default();
    let result = broker
        .fs_read(
            &FsRead::new("."),
            &allow_read(),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("directory read");
    assert!(result.truncated);
    assert!(result.preview.contains("[… 497 more .rs files]"));
    assert!(result.artifact.is_some());
    let Some(ToolResultData::FsRead {
        truncated_reason,
        entries_seen,
        collapsed_entries,
    }) = result.data
    else {
        panic!("typed read data");
    };
    assert_eq!(truncated_reason, Some(ToolTruncationReason::EntryLimit));
    assert_eq!(entries_seen, 511);
    assert!(collapsed_entries >= 497);
    assert_eq!(
        String::from_utf8_lossy(cas.0.last().expect("listing CAS"))
            .lines()
            .count(),
        500
    );
}

#[test]
fn typed_reason_names_remain_wire_stable() {
    assert_eq!(
        serde_json::to_string(&ToolTruncationReason::TimeBudget).expect("serialize"),
        "\"time_budget\""
    );
}
