//! Workspace guard: every pin the ledger counts must actually RUN.
//!
//! The ledger (`test-baseline.txt`, via `xtask test-count`) counts test
//! markers in any `tests/` file or any `*_tests.rs`. Cargo auto-discovers
//! `crates/*/tests/*.rs` as integration targets, so those always run — but a
//! `*_tests.rs` under `src/` is only compiled when some module DECLARES it
//! (`mod foo_tests;` or `#[path = "foo_tests.rs"] mod tests;`). An undeclared
//! one is invisible to the compiler and to `cargo test`, while still inflating
//! the ledger: phantom pins that can never fail.
//!
//! This is the sibling of the vacuous-pass lesson (a green test proves nothing
//! until you have seen it go red for the right reason): a test that never runs
//! cannot go red at all, and unlike a vacuous pass it leaves no trace in any
//! output to notice.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/haider-verify
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/<crate>")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// MUTATION CHECK (executed): drop the `#[path = "openai_tests.rs"]`
/// declaration from `haider-provider/src/openai.rs` — 117 real pins stop
/// compiling and stop running while the ledger still counts them; this guard
/// is what reports it.
#[test]
fn every_src_test_file_is_declared_by_a_module() {
    let root = workspace_root();
    let crates = root.join("crates");
    let mut undeclared = Vec::new();

    for entry in fs::read_dir(&crates).expect("crates dir").flatten() {
        let src = entry.path().join("src");
        if !src.is_dir() {
            continue;
        }
        let mut sources = Vec::new();
        rust_sources(&src, &mut sources);

        // Every `*_tests.rs` under src/ ...
        let test_files: Vec<&PathBuf> = sources
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("_tests.rs"))
            })
            .collect();
        if test_files.is_empty() {
            continue;
        }
        // ... must be named by some OTHER source file in the same crate,
        // either as `mod foo_tests;` or `#[path = "foo_tests.rs"]`.
        let declarations: String = sources
            .iter()
            .filter(|path| {
                !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("_tests.rs"))
            })
            .filter_map(|path| fs::read_to_string(path).ok())
            .collect();

        for file in test_files {
            let name = file
                .file_name()
                .and_then(|name| name.to_str())
                .expect("utf-8 file name");
            let stem = name.trim_end_matches(".rs");
            let declared = declarations.contains(&format!("mod {stem};"))
                || declarations.contains(&format!("mod {stem} "))
                || declarations.contains(&format!("path = \"{name}\""));
            if !declared {
                undeclared.push(file.clone());
            }
        }
    }

    assert!(
        undeclared.is_empty(),
        "these src test files are counted by the ledger but no module declares \
         them, so they never compile and never run — declare each with `mod \
         <name>;` or `#[path = \"<name>.rs\"] mod tests;`, or delete it: {undeclared:#?}"
    );
}
