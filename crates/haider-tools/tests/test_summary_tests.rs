//! `test_run` summarizer laws over REAL captured console transcripts
//! (fixtures under `tests/fixtures/testsummary/`, captured 2026-09-17 from
//! cargo 1.95.0, Python 3.12/3.9 unittest, pytest 8.4.2, and Gradle with
//! JUnit Jupiter 5.10.2): exact counts per format, verbatim failing-test
//! output, honest `unknown` degradation (counts are never fabricated),
//! deterministic rendering with the `cap:<call_id>` alias only, and
//! redaction happening upstream of (and surviving) summarization.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_tools::{
    ProcessOutputChunk, TEST_RUN_FAILURE_DETAIL_MAX_BYTES, TestCounts, TestOutputFormat,
    TestRunRenderContext, TestRunSummary, redact_process_output, render_test_run_output,
    summarize_test_output,
};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/testsummary")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn context<'a>(call_id: &'a str, output: &str) -> TestRunRenderContext<'a> {
    TestRunRenderContext {
        call_id,
        exit_code: Some(1),
        output_bytes: output.len() as u64,
        source_unavailable_bytes_at_least: 0,
        capture_retained: true,
        limit_reached: None,
    }
}

#[test]
fn cargo_mixed_run_counts_and_verbatim_failures() {
    let output = fixture("cargo_mixed.txt");
    let summary = summarize_test_output(&output);
    assert_eq!(summary.format, TestOutputFormat::CargoTest);
    assert_eq!(
        summary.counts,
        Some(TestCounts {
            passed: 2,
            failed: 2,
            ignored: 1
        })
    );
    let names: Vec<&str> = summary
        .failures
        .iter()
        .map(|failure| failure.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["tests::addition_is_wrong_here", "tests::subtraction_panics"]
    );
    // VERBATIM per-test output — the wishlist's exact ask.
    assert!(
        summary.failures[0]
            .detail
            .contains("assertion `left == right` failed: two plus two should equal five"),
        "{:?}",
        summary.failures[0]
    );
    assert!(summary.failures[0].detail.contains("  left: 4"));
    assert!(
        summary.failures[1]
            .detail
            .contains("index out of bounds: the len is 0 but the index is 3")
    );
    let rendered = render_test_run_output(&summary, &output, &context("call-7", &output));
    assert!(
        rendered
            .starts_with("test_run summary (format=cargo_test): 2 passed, 2 failed, 1 ignored\n"),
        "{rendered}"
    );
    assert!(rendered.contains("--- tests::subtraction_panics ---"));
    // This transcript fits the inline-complete window: it is embedded whole
    // and needs no paging pointer at all (row-63 discipline).
    assert!(rendered.contains("full output (complete):"));
    assert!(!rendered.contains("task_output("));
    assert!(!rendered.contains("capture:effect"));
}

#[test]
fn cargo_zero_test_run_sums_all_suites_to_zero() {
    let output = fixture("cargo_zero.txt");
    let summary = summarize_test_output(&output);
    assert_eq!(summary.format, TestOutputFormat::CargoTest);
    assert_eq!(summary.counts, Some(TestCounts::default()));
    assert!(summary.failures.is_empty());
    let mut context = context("call-zero", &output);
    context.exit_code = Some(0);
    let rendered = render_test_run_output(&summary, &output, &context);
    assert!(rendered.contains("0 passed, 0 failed, 0 ignored"));
    // A sub-window transcript is embedded whole; nothing needs paging.
    assert!(rendered.contains("full output (complete):"));
    assert!(!rendered.contains("task_output("));
}

#[test]
fn compilation_error_before_tests_degrades_honestly() {
    let output = fixture("cargo_err.txt");
    let summary = summarize_test_output(&output);
    assert_eq!(summary.format, TestOutputFormat::Unknown);
    assert_eq!(summary.counts, None, "counts must never be fabricated");
    assert!(summary.failures.is_empty());
    let mut context = context("call-err", &output);
    context.exit_code = Some(101);
    let rendered = render_test_run_output(&summary, &output, &context);
    assert!(rendered.contains("format=unknown"), "{rendered}");
    assert!(rendered.contains("exit_code=101"));
    assert!(rendered.contains("counts unavailable (never inferred)"));
    // The bounded window carries the real compiler diagnosis.
    assert!(rendered.contains("error[E0308]"));
}

#[test]
fn unittest_mixed_run_parses_failures_and_errors() {
    let output = fixture("unittest_mixed.txt");
    let summary = summarize_test_output(&output);
    assert_eq!(summary.format, TestOutputFormat::PythonUnittest);
    assert_eq!(
        summary.counts,
        Some(TestCounts {
            passed: 2,
            failed: 2,
            ignored: 1
        })
    );
    let names: Vec<&str> = summary
        .failures
        .iter()
        .map(|failure| failure.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "test_lookup_missing_sku_fails (test_inventory.InventoryTests)",
            "test_price_rounding_is_wrong_here (test_inventory.InventoryTests)",
        ]
    );
    assert!(summary.failures[0].detail.contains("KeyError: 'sku-2'"));
    assert!(
        summary.failures[1]
            .detail
            .contains("AssertionError: 2.67 != 2.68 within 7 places")
    );
}

#[test]
fn fully_passing_verbose_unittest_stays_inline_with_counts() {
    // AX-1 F3's shape: every line ends "... ok"; nothing must be hidden
    // behind a paging round-trip, and counts must still be real.
    let output = fixture("unittest_pass.txt");
    let summary = summarize_test_output(&output);
    assert_eq!(summary.format, TestOutputFormat::PythonUnittest);
    assert_eq!(
        summary.counts,
        Some(TestCounts {
            passed: 3,
            failed: 0,
            ignored: 0
        })
    );
    assert!(summary.failures.is_empty());
    let mut context = context("call-pass", &output);
    context.exit_code = Some(0);
    let rendered = render_test_run_output(&summary, &output, &context);
    assert!(rendered.contains("3 passed, 0 failed, 0 ignored"));
    assert!(rendered.contains("full output (complete):"));
    assert!(rendered.contains("test_alpha (test_passing.PassingTests) ... ok"));
    assert!(!rendered.contains("task_output("));
}

#[test]
fn pytest_mixed_run_counts_and_failure_blocks() {
    let output = fixture("pytest_mixed.txt");
    let summary = summarize_test_output(&output);
    assert_eq!(summary.format, TestOutputFormat::Pytest);
    assert_eq!(
        summary.counts,
        Some(TestCounts {
            passed: 2,
            failed: 2,
            ignored: 1
        })
    );
    let names: Vec<&str> = summary
        .failures
        .iter()
        .map(|failure| failure.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["test_discount_is_wrong_here", "test_negative_total_raises"]
    );
    assert!(
        summary.failures[0]
            .detail
            .contains("AssertionError: ten percent off 200 should be 180")
    );
    assert!(
        summary.failures[1]
            .detail
            .contains("Failed: DID NOT RAISE <class 'ValueError'>")
    );
}

#[test]
fn gradle_junit_console_counts_and_failed_tests() {
    let output = fixture("gradle_junit.txt");
    let summary = summarize_test_output(&output);
    assert_eq!(summary.format, TestOutputFormat::GradleJunit);
    assert_eq!(
        summary.counts,
        Some(TestCounts {
            passed: 2,
            failed: 2,
            ignored: 1
        })
    );
    let names: Vec<&str> = summary
        .failures
        .iter()
        .map(|failure| failure.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "LedgerTest > overdraftDetected()",
            "LedgerTest > withdrawalIsWrongHere()",
        ]
    );
    assert!(
        summary.failures[0]
            .detail
            .contains("org.opentest4j.AssertionFailedError at LedgerTest.java:27")
    );
}

#[test]
fn gradle_passing_console_reports_no_counts_rather_than_guessing() {
    // A fully passing Gradle test task prints no aggregate tally on the
    // plain console; the summary must say so instead of inventing one.
    let output = "> Task :compileTestJava\n> Task :test\n\nBUILD SUCCESSFUL in 4s\n2 actionable tasks: 2 executed\n";
    let summary = summarize_test_output(output);
    assert_eq!(summary.format, TestOutputFormat::GradleJunit);
    assert_eq!(summary.counts, None);
    assert!(summary.failures.is_empty());
    let mut context = context("call-gradle", output);
    context.exit_code = Some(0);
    let rendered = render_test_run_output(&summary, output, &context);
    assert!(
        rendered.contains("recognized output reports no aggregate counts"),
        "{rendered}"
    );
    assert!(!rendered.contains(" passed,"));
}

#[test]
fn unknown_output_never_gains_counts() {
    let output = "hello world\nnothing that resembles a test summary\n";
    let summary = summarize_test_output(output);
    assert_eq!(summary, TestRunSummary::unknown());
    let rendered = render_test_run_output(&summary, output, &context("call-u", output));
    assert!(rendered.contains("format=unknown"));
    assert!(!rendered.contains(" passed"));
}

#[test]
fn summarize_and_render_are_deterministic() {
    let output = fixture("cargo_mixed.txt");
    let mut context = context("call-det", &output);
    // Force the incomplete-display shape so the paging pointer appears.
    context.source_unavailable_bytes_at_least = 1;
    let first = render_test_run_output(&summarize_test_output(&output), &output, &context);
    let second = render_test_run_output(&summarize_test_output(&output), &output, &context);
    assert_eq!(first, second);
    // The only capture identity in the provider-facing text is the
    // deterministic conversation-local alias (AHRB row 63).
    assert!(first.contains("cap:call-det"));
    assert!(!first.contains("capture:"));
}

#[test]
fn redacted_secrets_stay_redacted_through_the_summary() {
    // The summarizer consumes the SAME redaction consumer's output as every
    // process result; a secret in a failing test's output must arrive as the
    // standard marker, verbatim within the failure detail.
    let raw = concat!(
        "running 1 tests\n",
        "test tests::leaks_a_secret ... FAILED\n",
        "\n",
        "failures:\n",
        "\n",
        "---- tests::leaks_a_secret stdout ----\n",
        "token=sk-abcdefghijklmnopQRSTUV\n",
        "assertion failed: credentials rejected\n",
        "\n",
        "failures:\n",
        "    tests::leaks_a_secret\n",
        "\n",
        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n",
    );
    let chunks = [ProcessOutputChunk {
        stream: haider_protocol::item::OutputStream::Stdout,
        chunk_b64: BASE64.encode(raw),
    }];
    let safe = redact_process_output(&chunks).expect("redacted transcript");
    assert!(!safe.contains("sk-abcdefghijklmnopQRSTUV"));
    let summary = summarize_test_output(&safe);
    assert_eq!(summary.format, TestOutputFormat::CargoTest);
    assert_eq!(summary.failures.len(), 1);
    assert!(summary.failures[0].detail.contains("[REDACTED"));
    assert!(!summary.failures[0].detail.contains("sk-abcdefghijklmnop"));
    assert!(
        summary.failures[0]
            .detail
            .contains("assertion failed: credentials rejected")
    );
}

#[test]
fn oversized_failure_detail_is_elided_with_exact_accounting() {
    let noise = "assertion context line with padding padding padding\n".repeat(600);
    let output = format!(
        "running 1 tests\ntest tests::huge ... FAILED\n\nfailures:\n\n---- tests::huge stdout ----\n{noise}\nfailures:\n    tests::huge\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"
    );
    let summary = summarize_test_output(&output);
    assert_eq!(summary.counts.expect("counts").failed, 1);
    let rendered = render_test_run_output(&summary, &output, &context("call-big", &output));
    assert!(rendered.contains("test_run_failure_detail"), "{rendered}");
    assert!(rendered.contains("haider_elision_v1"));
    assert!(rendered.len() < TEST_RUN_FAILURE_DETAIL_MAX_BYTES + 4096);
    assert!(rendered.contains("cap:call-big"));
}

#[test]
fn nonzero_exit_with_clean_summary_is_flagged_not_hidden() {
    // e.g. a later crate fails to compile after one suite passed, or the
    // harness dies after printing a green tally: the exit code wins.
    let output = "running 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\nerror: could not compile `later-crate` (lib) due to 1 previous error\n".repeat(60);
    let summary = summarize_test_output(&output);
    assert_eq!(summary.format, TestOutputFormat::CargoTest);
    let mut context = context("call-guard", &output);
    context.exit_code = Some(101);
    let rendered = render_test_run_output(&summary, &output, &context);
    assert!(
        rendered.contains("failure outside the recognized test summary (exit_code=101)"),
        "{rendered}"
    );
    assert!(rendered.contains("output tail:"));
    assert!(rendered.contains("cap:call-guard"));
}

#[test]
fn early_limit_skips_parsing_and_says_so() {
    let output = "running 500 tests\ntest a ... ok\n";
    let summary = TestRunSummary::unknown();
    let mut context = context("call-limit", output);
    context.limit_reached = Some("wall_timeout");
    context.source_unavailable_bytes_at_least = 4096;
    let rendered = render_test_run_output(&summary, output, &context);
    assert!(rendered.contains("run terminated early by wall_timeout"));
    assert!(rendered.contains("partial capture is authoritative"));
    assert!(rendered.contains("cap:call-limit"));
    assert!(
        rendered.contains("At least 4096 further bytes arrived after the output limit"),
        "{rendered}"
    );
}
