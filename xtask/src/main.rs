//! Repo guard tooling (workspace rules from BUILDGUIDE):
//! - `xtask loc-lint`     — soft 10k-LOC cap per source file (warns, never fails).
//! - `xtask test-count`   — fails CI if the workspace test count DROPS below the
//!   committed baseline (`test-baseline.txt`); `--update` rewrites the baseline.
//! - `xtask check`        — both.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const LOC_SOFT_CAP: usize = 10_000;
const BASELINE_FILE: &str = "test-baseline.txt";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("loc-lint") => loc_lint(),
        Some("test-count") => test_count(args.iter().any(|a| a == "--update")),
        Some("check") => {
            let a = loc_lint();
            let b = test_count(false);
            if a == ExitCode::SUCCESS && b == ExitCode::SUCCESS {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => {
            eprintln!("usage: xtask <loc-lint|test-count [--update]|check>");
            ExitCode::from(2)
        }
    }
}

fn workspace_root() -> PathBuf {
    // xtask always runs from the workspace (cargo sets CARGO_MANIFEST_DIR to xtask/).
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            if name != "target" && name != ".git" && name != "node_modules" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn loc_lint() -> ExitCode {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut over = 0usize;
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let lines = text.lines().count();
        if lines > LOC_SOFT_CAP {
            over += 1;
            eprintln!(
                "loc-lint: WARNING {} has {lines} lines (soft cap {LOC_SOFT_CAP})",
                file.display()
            );
        }
    }
    println!(
        "loc-lint: {} files scanned, {over} over the soft cap",
        files.len()
    );
    ExitCode::SUCCESS // soft cap: warn only, by design
}

fn count_tests(root: &Path) -> usize {
    let mut files = Vec::new();
    rust_files(root, &mut files);
    files
        .iter()
        // Workspace rule: tests live in tests/ dirs (and *_tests.rs files), never inline.
        .filter(|p| {
            p.components().any(|c| c.as_os_str() == "tests")
                || p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("_tests.rs"))
        })
        .filter_map(|p| fs::read_to_string(p).ok())
        // `#[tokio::test` (no closing bracket) also catches the CONFIGURED
        // forms — `#[tokio::test(start_paused = true)]`, `flavor = …` — which
        // an exact `#[tokio::test]` match silently skipped, undercounting
        // every paused-time driver test.
        .map(|text| text.matches("#[test]").count() + text.matches("#[tokio::test").count())
        .sum()
}

fn test_count(update: bool) -> ExitCode {
    let root = workspace_root();
    let current = count_tests(&root);
    let baseline_path = root.join(BASELINE_FILE);
    if update {
        if fs::write(&baseline_path, format!("{current}\n")).is_err() {
            eprintln!("test-count: cannot write {BASELINE_FILE}");
            return ExitCode::FAILURE;
        }
        println!("test-count: baseline updated to {current}");
        return ExitCode::SUCCESS;
    }
    let baseline: usize = fs::read_to_string(&baseline_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if current < baseline {
        eprintln!(
            "test-count: FAIL — {current} tests found, baseline is {baseline}. \
             Reducing tests requires an explicit reviewed waiver (rerun with --update in the same patch)."
        );
        return ExitCode::FAILURE;
    }
    println!("test-count: {current} tests (baseline {baseline}) — ok");
    ExitCode::SUCCESS
}
