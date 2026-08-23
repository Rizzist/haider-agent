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

/// Counts EVERY test in the workspace, wherever it lives.
///
/// This counter previously filtered to `tests/` directories and `*_tests.rs`
/// files, on the workspace convention that tests live there rather than inline.
/// That made it blind to **211 tests (6.7% of the suite)** — including the pins
/// guarding the OAuth cache fixes, whose last regression cost four releases, and
/// the pins on the wildcard-recursion crash fix.
///
/// The blindness mattered because of what this guard PROMISES. Its failure
/// message says reducing tests "requires an explicit reviewed waiver" — and for
/// those 211 it could not deliver that: delete any of them and the count stayed
/// green. A guard that is honest about what it examined, read as a statement
/// about what exists.
///
/// Widening the counter is the right fix rather than relocating 211 tests. The
/// guard's PURPOSE is preventing silent test deletion, and it serves that better
/// by counting everything. The convention can remain a style preference enforced
/// by review; it should not be the thing the arithmetic depends on.
///
/// The position rule below still applies, so prose mentioning a marker and
/// markers embedded mid-line are still excluded — widening WHERE we look does
/// not widen WHAT counts as a test.
fn count_tests(root: &Path) -> usize {
    let mut files = Vec::new();
    rust_files(root, &mut files);
    files
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .map(|text| text.lines().filter(|line| is_test_marker(line)).count())
        .sum()
}

/// Test markers this counter recognises. Deliberately exact strings: widening
/// the SET (say, to attributes carrying arguments) is a separate change from
/// the POSITION rule below, so the two compose instead of overwriting one
/// another.
// `#[tokio::test` (no closing bracket) is a PREFIX marker: it also counts
// the CONFIGURED forms — `#[tokio::test(start_paused = true)]`, `flavor = …`
// — which an exact match silently skipped (TUI3b found 13 uncounted tests).
const TEST_MARKERS: [&str; 2] = ["#[test]", "#[tokio::test"];

/// Counting rule, stated once: a marker counts only where an attribute can
/// actually appear — as the first token on its line. That excludes prose that
/// merely mentions a marker (a doc comment describing the acceptance matrix is
/// not a test) and any marker embedded mid-line, such as this counter's own
/// source. Substring counting had exactly that bug: `lifecycle_tests.rs`'s
/// header, which names `#[tokio::test]` in a sentence, inflated the workspace
/// baseline by one phantom test.
fn is_test_marker(line: &str) -> bool {
    let trimmed = line.trim_start();
    TEST_MARKERS
        .iter()
        .any(|marker| trimmed.starts_with(marker))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// This test is deliberately INLINE — in `main.rs`, not a `tests/` dir and
    /// not a `*_tests.rs` file. Before the counter was widened it would have
    /// been invisible to the very guard it exercises, which is the point: the
    /// pin and the bug share a location.
    ///
    /// MUTATION: restore the old filter in `count_tests`
    ///     .filter(|p| p.components().any(|c| c.as_os_str() == "tests") || ...)
    /// and this test fails with 0 found instead of 2 — which is exactly the
    /// silent re-blinding it exists to prevent.
    #[test]
    fn count_tests_sees_tests_that_are_not_in_a_tests_directory() {
        let dir = std::env::temp_dir().join(format!(
            "haider-xtask-count-{}-{}",
            std::process::id(),
            line!()
        ));
        let src = dir.join("crates").join("thing").join("src");
        fs::create_dir_all(&src).expect("create fixture tree");

        // An ordinary source module with inline tests: the 211-test blind spot.
        fs::write(
            src.join("inline.rs"),
            "fn f() {}\n#[test]\nfn a() {}\n#[tokio::test]\nasync fn b() {}\n",
        )
        .expect("write inline fixture");

        // Prose naming a marker must still NOT count: widening WHERE we look
        // must not widen WHAT counts.
        fs::write(
            src.join("prose.rs"),
            "//! This module documents #[test] usage.\n/// See #[tokio::test] for async.\nfn g() {}\n",
        )
        .expect("write prose fixture");

        let found = count_tests(&dir);
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            found, 2,
            "inline #[test] and #[tokio::test] must count, and prose mentioning \
             a marker must not"
        );
    }
}
