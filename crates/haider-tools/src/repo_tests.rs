#![allow(clippy::expect_used)]

use super::{WalkOptions, detect_repo_root, walk_files};
use std::fs;
use std::time::Instant;

#[test]
fn repository_root_and_nested_ignore_rules_remain_workspace_confined() {
    let outside = tempfile::tempdir().expect("outside");
    fs::create_dir(outside.path().join(".git")).expect("outside git marker");
    let workspace = outside.path().join("workspace");
    fs::create_dir_all(workspace.join("repo/src/generated")).expect("tree");
    fs::create_dir(workspace.join("repo/.git")).expect("repo marker");
    fs::create_dir_all(workspace.join("repo/.git/info")).expect("exclude directory");
    fs::write(workspace.join("repo/.gitignore"), "ignored.txt\n").expect("root ignore");
    fs::write(workspace.join("repo/.ignore"), "!ignored.txt\n").expect("ripgrep ignore");
    fs::write(workspace.join("repo/.git/info/exclude"), "excluded.rs\n").expect("git exclude");
    fs::write(
        workspace.join("repo/src/.gitignore"),
        "generated/*\n!generated/keep.rs\n",
    )
    .expect("nested ignore");
    fs::write(workspace.join("repo/src/visible.rs"), "visible").expect("visible");
    fs::write(workspace.join("repo/src/ignored.txt"), "ignored").expect("ignored");
    fs::write(workspace.join("repo/src/excluded.rs"), "excluded").expect("excluded");
    fs::write(workspace.join("repo/src/generated/drop.rs"), "drop").expect("drop");
    fs::write(workspace.join("repo/src/generated/keep.rs"), "keep").expect("keep");

    assert_eq!(
        detect_repo_root(&workspace, &workspace.join("repo/src")),
        Some(workspace.join("repo"))
    );
    assert_eq!(
        detect_repo_root(&workspace, &workspace),
        None,
        "the parent repository marker must never be consulted"
    );
    let walked = walk_files(
        &workspace,
        &workspace.join("repo/src"),
        WalkOptions {
            respect_gitignore: true,
            include_hidden: false,
            max_files: 100,
            deadline: None,
        },
    )
    .expect("walk");
    assert_eq!(
        walked.files,
        vec![
            std::path::PathBuf::from("repo/src/generated/keep.rs"),
            std::path::PathBuf::from("repo/src/visible.rs"),
        ]
    );
}

#[cfg(unix)]
#[test]
fn symlinked_gitignore_controls_are_never_followed() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("workspace");
    fs::create_dir(workspace.path().join(".git")).expect("git marker");
    let outside = tempfile::NamedTempFile::new().expect("outside ignore");
    symlink(outside.path(), workspace.path().join(".gitignore")).expect("ignore symlink");
    let error = walk_files(
        workspace.path(),
        workspace.path(),
        WalkOptions {
            respect_gitignore: true,
            include_hidden: true,
            max_files: 100,
            deadline: None,
        },
    )
    .expect_err("symlinked ignore is rejected");
    assert!(matches!(error, crate::ToolError::PathChanged { .. }));
}

#[cfg(unix)]
#[test]
fn symlinked_git_metadata_ancestors_are_never_opened() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside metadata");
    fs::create_dir(workspace.path().join(".git")).expect("git marker");
    symlink(outside.path(), workspace.path().join(".git/info")).expect("info symlink");
    let error = walk_files(
        workspace.path(),
        workspace.path(),
        WalkOptions {
            respect_gitignore: true,
            include_hidden: true,
            max_files: 100,
            deadline: None,
        },
    )
    .expect_err("symlinked metadata ancestor is rejected by anchored open");
    assert!(matches!(error, crate::ToolError::PathChanged { .. }));

    let workspace = tempfile::tempdir().expect("workspace");
    fs::write(outside.path().join("exclude"), "visible.txt\n").expect("outside exclude");
    symlink(outside.path(), workspace.path().join(".git")).expect("git symlink");
    fs::write(workspace.path().join("visible.txt"), "visible").expect("visible");
    let walked = walk_files(
        workspace.path(),
        workspace.path(),
        WalkOptions {
            respect_gitignore: true,
            include_hidden: true,
            max_files: 100,
            deadline: None,
        },
    )
    .expect("metadata symlink is excluded, not followed");
    assert_eq!(walked.files, vec![std::path::PathBuf::from("visible.txt")]);
}

#[test]
fn hidden_policy_and_file_cap_are_deterministic() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::write(workspace.path().join(".hidden"), "hidden").expect("hidden");
    fs::write(workspace.path().join("b.txt"), "b").expect("b");
    fs::write(workspace.path().join("a.txt"), "a").expect("a");
    let walked = walk_files(
        workspace.path(),
        workspace.path(),
        WalkOptions {
            respect_gitignore: false,
            include_hidden: false,
            max_files: 1,
            deadline: None,
        },
    )
    .expect("walk");
    assert_eq!(walked.files, vec![std::path::PathBuf::from("a.txt")]);
    assert!(walked.truncated);
}

#[test]
fn hidden_entries_consume_the_enumeration_cap() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::create_dir(workspace.path().join(".hidden")).expect("hidden directory");
    fs::write(workspace.path().join(".hidden/one"), "one").expect("hidden file");
    let walked = walk_files(
        workspace.path(),
        workspace.path(),
        WalkOptions {
            respect_gitignore: false,
            include_hidden: false,
            max_files: 1,
            deadline: None,
        },
    )
    .expect("bounded walk");
    assert!(walked.truncated);
    assert!(walked.files.is_empty());
}

#[test]
fn ignore_controls_precede_caps_and_strip_utf8_bom() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::write(workspace.path().join(".gitignore"), "\u{feff}secret.txt\n").expect("BOM ignore");
    fs::write(workspace.path().join("secret.txt"), "secret").expect("ignored file");
    let walked = walk_files(
        workspace.path(),
        workspace.path(),
        WalkOptions {
            respect_gitignore: true,
            include_hidden: true,
            max_files: 100,
            deadline: None,
        },
    )
    .expect("BOM-aware walk");
    assert!(
        !walked
            .files
            .contains(&std::path::PathBuf::from("secret.txt"))
    );

    for index in 0..10 {
        fs::write(workspace.path().join(format!(".aaa-{index}")), "hidden").expect("hidden");
    }
    let capped = walk_files(
        workspace.path(),
        workspace.path(),
        WalkOptions {
            respect_gitignore: true,
            include_hidden: true,
            max_files: 1,
            deadline: None,
        },
    )
    .expect("control-first capped walk");
    assert!(capped.truncated);
    assert!(
        !capped
            .files
            .contains(&std::path::PathBuf::from("secret.txt"))
    );
}

#[test]
fn dot_git_is_always_excluded_and_noncanonical_roots_are_rejected() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::create_dir(workspace.path().join(".GIT")).expect("mixed-case metadata");
    fs::write(workspace.path().join(".GIT/config"), "token=never-list").expect("config");
    fs::write(workspace.path().join("visible.txt"), "visible").expect("visible");
    let walked = walk_files(
        workspace.path(),
        workspace.path(),
        WalkOptions {
            respect_gitignore: false,
            include_hidden: true,
            max_files: 100,
            deadline: Some(Instant::now() + std::time::Duration::from_secs(1)),
        },
    )
    .expect("walk");
    assert_eq!(walked.files, vec![std::path::PathBuf::from("visible.txt")]);

    let escaped = workspace.path().join("..");
    assert!(
        walk_files(
            workspace.path(),
            &escaped,
            WalkOptions {
                respect_gitignore: false,
                include_hidden: true,
                max_files: 100,
                deadline: None,
            },
        )
        .is_err()
    );
}
