//! Deterministic test-output summarization for the `test_run` tool.
//!
//! The summarizer is a pure function over the COMPLETE secret-redacted
//! capture (the same single redaction consumer every process result uses).
//! It recognizes well-known console formats through an extensible parser
//! table and NEVER fabricates counts: an unrecognized shape degrades to
//! `format = unknown` with the exit code and a bounded verbatim tail. The
//! renderer produces provider-facing text that is byte-identical across
//! sessions (AHRB row 63): its only capture pointer is the deterministic
//! conversation-local `cap:<call_id>` alias.

use crate::process::capture_paging_hint;
use crate::shell::{PROCESS_INLINE_RETENTION_BYTES, strip_ansi};
use haider_protocol::context::elide_text_head_tail;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// Wall-clock limit for one foreground `test_run` invocation. Real suites
/// routinely exceed the 60-second `process_exec` default; the point of the
/// affordance is one call per run.
pub const TEST_RUN_WALL_TIMEOUT_SECS: u64 = 600;
/// Combined-output termination threshold for `test_run`. Verbose suite logs
/// exceed the 1 MiB `process_exec` default without being abusive; the
/// process layer's hard 2 MiB ceiling is the honest upper bound.
pub const TEST_RUN_MAX_OUTPUT_BYTES: usize = crate::process::PROCESS_MAX_OUTPUT_BYTES;
/// Generous verbatim budget for the failing-test section of the summary.
pub const TEST_RUN_FAILURE_DETAIL_MAX_BYTES: usize = 12 * 1024;
/// Bounded verbatim window shown for unrecognized output.
pub const TEST_RUN_UNKNOWN_TAIL_MAX_BYTES: usize = 2 * 1024;
/// Cap on individually listed failing tests; the capture holds the rest.
pub const TEST_RUN_MAX_LISTED_FAILURES: usize = 50;

/// The `test_run` execution bounds: `process_exec` defaults with the wall and
/// output limits above. Same broker, same policy, same capture pipeline.
#[must_use]
pub fn test_run_process_bounds() -> crate::ProcessBounds {
    crate::ProcessBounds {
        max_output_bytes: TEST_RUN_MAX_OUTPUT_BYTES,
        wall_timeout: std::time::Duration::from_secs(TEST_RUN_WALL_TIMEOUT_SECS),
        ..Default::default()
    }
}

/// Recognized console formats, in detection order. Extending the affordance
/// to a new runner means adding a variant plus one `TestOutputParserSpec`
/// row — nothing else changes shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutputFormat {
    /// Rust libtest console (`cargo test` and compatible harnesses).
    CargoTest,
    /// pytest console.
    Pytest,
    /// `python -m unittest` console.
    PythonUnittest,
    /// Gradle test-task console (JUnit platform).
    GradleJunit,
    /// No recognized format: exit code + bounded tail, no counts.
    Unknown,
}

impl TestOutputFormat {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CargoTest => "cargo_test",
            Self::Pytest => "pytest",
            Self::PythonUnittest => "python_unittest",
            Self::GradleJunit => "gradle_junit",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCounts {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
}

/// One failing test: its reported name and its VERBATIM per-test output
/// (already secret-redacted upstream; the summarizer only selects lines).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestFailure {
    pub name: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRunSummary {
    pub format: TestOutputFormat,
    /// `None` whenever the output carries no parseable aggregate counts —
    /// counts are parsed, never inferred from exit codes or failure lists.
    pub counts: Option<TestCounts>,
    pub failures: Vec<TestFailure>,
}

impl TestRunSummary {
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            format: TestOutputFormat::Unknown,
            counts: None,
            failures: Vec::new(),
        }
    }
}

struct ParsedTestOutput {
    counts: Option<TestCounts>,
    failures: Vec<TestFailure>,
}

struct TestOutputParserSpec {
    format: TestOutputFormat,
    detect: fn(&str) -> bool,
    parse: fn(&str) -> Option<ParsedTestOutput>,
}

/// Detection order: most-specific banners first. Each parser must return
/// `None` rather than guess when its shape does not hold, so the summary
/// falls through to the honest `unknown` degradation.
const TEST_OUTPUT_PARSERS: &[TestOutputParserSpec] = &[
    TestOutputParserSpec {
        format: TestOutputFormat::CargoTest,
        detect: detect_cargo,
        parse: parse_cargo,
    },
    TestOutputParserSpec {
        format: TestOutputFormat::Pytest,
        detect: detect_pytest,
        parse: parse_pytest,
    },
    TestOutputParserSpec {
        format: TestOutputFormat::PythonUnittest,
        detect: detect_unittest,
        parse: parse_unittest,
    },
    TestOutputParserSpec {
        format: TestOutputFormat::GradleJunit,
        detect: detect_gradle,
        parse: parse_gradle,
    },
];

/// Summarizes one complete secret-redacted test transcript. Reapplying it to
/// the same bytes always produces an identical summary.
#[must_use]
pub fn summarize_test_output(output: &str) -> TestRunSummary {
    let stripped = strip_ansi(output);
    for spec in TEST_OUTPUT_PARSERS {
        if (spec.detect)(&stripped)
            && let Some(parsed) = (spec.parse)(&stripped)
        {
            return TestRunSummary {
                format: spec.format,
                counts: parsed.counts,
                failures: parsed.failures,
            };
        }
    }
    TestRunSummary::unknown()
}

// ---------------------------------------------------------------- cargo test

fn detect_cargo(output: &str) -> bool {
    output.lines().any(|line| {
        line.starts_with("test result: ") || line.trim_start().starts_with("test result: ")
    })
}

fn parse_cargo(output: &str) -> Option<ParsedTestOutput> {
    let mut counts = TestCounts::default();
    let mut any_summary = false;
    for line in output.lines() {
        let Some(rest) = line.trim_start().strip_prefix("test result: ") else {
            continue;
        };
        // "FAILED. 2 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out; …"
        let Some((_verdict, tallies)) = rest.split_once(". ") else {
            continue;
        };
        for part in tallies.split(';') {
            let mut words = part.split_whitespace();
            let Some(count) = words.next().and_then(|word| word.parse::<u64>().ok()) else {
                continue;
            };
            match words.next() {
                Some("passed") => counts.passed = counts.passed.saturating_add(count),
                Some("failed") => counts.failed = counts.failed.saturating_add(count),
                Some("ignored") => counts.ignored = counts.ignored.saturating_add(count),
                _ => {}
            }
        }
        any_summary = true;
    }
    if !any_summary {
        return None;
    }
    // Per-test verbatim blocks: `---- <name> stdout ----` up to the next
    // block header, the final `failures:` name list, or a suite summary.
    let mut details: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in output.lines() {
        if let Some(inner) = line
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" ----"))
        {
            if let Some((name, block)) = current.take() {
                details.push((name, join_block(&block)));
            }
            let name = match inner.rsplit_once(' ') {
                Some((name, "stdout" | "stderr")) => name,
                _ => inner,
            };
            current = Some((name.to_owned(), Vec::new()));
            continue;
        }
        if line == "failures:" || line.starts_with("test result: ") {
            if let Some((name, block)) = current.take() {
                details.push((name, join_block(&block)));
            }
            continue;
        }
        if let Some((_, block)) = current.as_mut() {
            block.push(line);
        }
    }
    if let Some((name, block)) = current.take() {
        details.push((name, join_block(&block)));
    }
    let mut failures = Vec::new();
    for line in output.lines() {
        if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            let detail = details
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, detail)| detail.clone())
                .unwrap_or_default();
            push_unique_failure(&mut failures, name.to_owned(), detail);
        }
    }
    if failures.is_empty() {
        for (name, detail) in details {
            push_unique_failure(&mut failures, name, detail);
        }
    }
    Some(ParsedTestOutput {
        counts: Some(counts),
        failures,
    })
}

// -------------------------------------------------------------------- pytest

fn detect_pytest(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.starts_with('=') && line.contains("test session starts"))
}

fn parse_pytest(output: &str) -> Option<ParsedTestOutput> {
    let summary_line = output.lines().rev().find(|line| {
        line.starts_with('=')
            && line.ends_with('=')
            && !line.contains("test session starts")
            && (line.contains(" in ") || line.contains("no tests ran"))
    })?;
    let inner = summary_line.trim_matches('=').trim();
    let mut counts = TestCounts::default();
    let mut recognized = inner.starts_with("no tests ran");
    if !recognized {
        for token in inner.split(", ") {
            // The duration rides the final token: "1 skipped in 0.02s".
            let token = token.split(" in ").next().unwrap_or(token);
            let mut words = token.split_whitespace();
            let Some(count) = words.next().and_then(|word| word.parse::<u64>().ok()) else {
                continue;
            };
            match words.next() {
                Some("passed" | "xpassed") => {
                    counts.passed = counts.passed.saturating_add(count);
                    recognized = true;
                }
                Some("failed" | "error" | "errors") => {
                    counts.failed = counts.failed.saturating_add(count);
                    recognized = true;
                }
                Some("skipped" | "deselected" | "xfailed") => {
                    counts.ignored = counts.ignored.saturating_add(count);
                    recognized = true;
                }
                _ => {}
            }
        }
    }
    if !recognized {
        return None;
    }
    let mut failures = Vec::new();
    let mut in_failure_section = false;
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in output.lines() {
        if line.starts_with('=') && line.ends_with('=') && line.len() > 8 {
            if let Some((name, block)) = current.take() {
                push_unique_failure(&mut failures, name, join_block(&block));
            }
            let banner = line.trim_matches('=').trim();
            in_failure_section = banner == "FAILURES" || banner == "ERRORS";
            continue;
        }
        if !in_failure_section {
            continue;
        }
        if line.starts_with('_') && line.ends_with('_') && line.len() > 8 {
            if let Some((name, block)) = current.take() {
                push_unique_failure(&mut failures, name, join_block(&block));
            }
            current = Some((line.trim_matches('_').trim().to_owned(), Vec::new()));
            continue;
        }
        if let Some((_, block)) = current.as_mut() {
            block.push(line);
        }
    }
    if let Some((name, block)) = current.take() {
        push_unique_failure(&mut failures, name, join_block(&block));
    }
    if failures.is_empty() {
        // Quiet runs keep the one-line node ids from the short summary.
        for line in output.lines() {
            let Some(rest) = line
                .strip_prefix("FAILED ")
                .or_else(|| line.strip_prefix("ERROR "))
            else {
                continue;
            };
            let name = rest.split(" - ").next().unwrap_or(rest).trim();
            if !name.is_empty() {
                push_unique_failure(&mut failures, name.to_owned(), String::new());
            }
        }
    }
    Some(ParsedTestOutput {
        counts: Some(counts),
        failures,
    })
}

// ---------------------------------------------------------- python unittest

fn detect_unittest(output: &str) -> bool {
    let ran = output
        .lines()
        .any(|line| line.starts_with("Ran ") && line.contains(" test") && line.contains(" in "));
    ran && output
        .lines()
        .any(|line| line == "OK" || line.starts_with("OK (") || line.starts_with("FAILED ("))
}

fn parse_unittest(output: &str) -> Option<ParsedTestOutput> {
    let total = output.lines().find_map(|line| {
        line.strip_prefix("Ran ")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|word| word.parse::<u64>().ok())
    })?;
    let verdict = output
        .lines()
        .rev()
        .find(|line| line == &"OK" || line.starts_with("OK (") || line.starts_with("FAILED ("))?;
    let mut failed = 0_u64;
    let mut ignored = 0_u64;
    if let Some(inside) = verdict
        .split_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
    {
        for token in inside.split(", ") {
            let Some((label, count)) = token.rsplit_once('=') else {
                continue;
            };
            let Ok(count) = count.parse::<u64>() else {
                continue;
            };
            match label.trim() {
                "failures" | "errors" | "unexpected successes" => {
                    failed = failed.saturating_add(count);
                }
                "skipped" | "expected failures" => ignored = ignored.saturating_add(count),
                _ => {}
            }
        }
    }
    let counts = TestCounts {
        passed: total.saturating_sub(failed).saturating_sub(ignored),
        failed,
        ignored,
    };
    let mut failures = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let header = (line.starts_with("FAIL: ") || line.starts_with("ERROR: "))
            && index > 0
            && is_rule(lines[index - 1], '=');
        if !header {
            index += 1;
            continue;
        }
        let name = line
            .split_once(": ")
            .map_or(line, |(_, rest)| rest)
            .to_owned();
        let mut block = Vec::new();
        index += 1;
        // Skip the dashed rule under the header.
        if index < lines.len() && is_rule(lines[index], '-') {
            index += 1;
        }
        while index < lines.len()
            && !is_rule(lines[index], '=')
            && !lines[index].starts_with("Ran ")
        {
            block.push(lines[index]);
            index += 1;
        }
        while block
            .last()
            .is_some_and(|last| last.trim().is_empty() || is_rule(last, '-'))
        {
            block.pop();
        }
        push_unique_failure(&mut failures, name, join_block(&block));
    }
    Some(ParsedTestOutput {
        counts: Some(counts),
        failures,
    })
}

fn is_rule(line: &str, glyph: char) -> bool {
    line.len() >= 10 && line.chars().all(|value| value == glyph)
}

// ------------------------------------------------------ gradle test console

fn detect_gradle(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.starts_with("> Task ") && line.contains(":test"))
}

fn parse_gradle(output: &str) -> Option<ParsedTestOutput> {
    // "5 tests completed, 2 failed, 1 skipped" — printed only on failure;
    // a fully passing task reports no aggregate counts on this console.
    let mut counts = None;
    for line in output.lines() {
        let mut parts = line.trim().split(", ");
        let Some(first) = parts.next() else { continue };
        let mut words = first.split_whitespace();
        let Some(total) = words.next().and_then(|word| word.parse::<u64>().ok()) else {
            continue;
        };
        if words.next() != Some("tests") || words.next() != Some("completed") {
            continue;
        }
        let mut failed = 0_u64;
        let mut skipped = 0_u64;
        for part in parts {
            let mut words = part.split_whitespace();
            let Some(count) = words.next().and_then(|word| word.parse::<u64>().ok()) else {
                continue;
            };
            match words.next() {
                Some("failed") => failed = failed.saturating_add(count),
                Some("skipped") => skipped = skipped.saturating_add(count),
                _ => {}
            }
        }
        counts = Some(TestCounts {
            passed: total.saturating_sub(failed).saturating_sub(skipped),
            failed,
            ignored: skipped,
        });
        break;
    }
    // "LedgerTest > withdrawalIsWrongHere() FAILED" + indented cause lines.
    let mut failures = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in output.lines() {
        if let Some(name) = line
            .strip_suffix(" FAILED")
            .filter(|_| !line.starts_with('>') && !line.starts_with(char::is_whitespace))
            .filter(|name| name.contains(" > "))
        {
            if let Some((name, block)) = current.take() {
                push_unique_failure(&mut failures, name, join_block(&block));
            }
            current = Some((name.to_owned(), Vec::new()));
            continue;
        }
        let continues_block = line.starts_with(char::is_whitespace) && !line.trim().is_empty();
        if let Some((_, block)) = current.as_mut().filter(|_| continues_block) {
            block.push(line);
        } else if let Some((name, block)) = current.take() {
            push_unique_failure(&mut failures, name, join_block(&block));
        }
    }
    if let Some((name, block)) = current.take() {
        push_unique_failure(&mut failures, name, join_block(&block));
    }
    Some(ParsedTestOutput { counts, failures })
}

fn join_block(block: &[&str]) -> String {
    let mut lines: &[&str] = block;
    while let Some((first, rest)) = lines.split_first()
        && first.trim().is_empty()
    {
        lines = rest;
    }
    while let Some((last, rest)) = lines.split_last()
        && last.trim().is_empty()
    {
        lines = rest;
    }
    lines.join("\n")
}

fn push_unique_failure(failures: &mut Vec<TestFailure>, name: String, detail: String) {
    if failures
        .iter()
        .any(|failure| failure.name == name && failure.detail == detail)
    {
        return;
    }
    failures.push(TestFailure { name, detail });
}

// ------------------------------------------------------------------ renderer

/// Facts the renderer needs beyond the parsed summary. Every field is
/// deterministic for identical executions; no volatile identity may appear
/// here (the paging pointer names `cap:<call_id>` only).
#[derive(Debug, Clone, Copy)]
pub struct TestRunRenderContext<'a> {
    pub call_id: &'a str,
    pub exit_code: Option<i32>,
    /// Raw combined-output byte count from the process result.
    pub output_bytes: u64,
    /// Bytes that arrived after the output limit and were never captured.
    pub source_unavailable_bytes_at_least: u64,
    /// Whether a session-scoped capture was retained for paging.
    pub capture_retained: bool,
    /// The execution limit that terminated the run early, if any
    /// (e.g. "wall_timeout"). Parsing is skipped for a cut-off transcript.
    pub limit_reached: Option<&'a str>,
}

/// Renders the deterministic provider-facing `output` text for a `test_run`
/// result: counts header, verbatim failing tests within a generous bound,
/// honest degradation for unknown/cut-off output, and either the complete
/// output inline (small runs) or the `cap:<call_id>` paging pointer.
#[must_use]
pub fn render_test_run_output(
    summary: &TestRunSummary,
    safe_output: &str,
    context: &TestRunRenderContext<'_>,
) -> String {
    let mut out = String::new();
    let exit = context
        .exit_code
        .map_or_else(|| "none".to_owned(), |code| code.to_string());
    match (summary.format, summary.counts.as_ref()) {
        (TestOutputFormat::Unknown, _) => {
            let _ = writeln!(
                out,
                "test_run summary (format=unknown): unrecognized test output; exit_code={exit}; counts unavailable (never inferred)"
            );
        }
        (format, Some(counts)) => {
            let _ = writeln!(
                out,
                "test_run summary (format={}): {} passed, {} failed, {} ignored",
                format.name(),
                counts.passed,
                counts.failed,
                counts.ignored
            );
        }
        (format, None) => {
            let _ = writeln!(
                out,
                "test_run summary (format={}): recognized output reports no aggregate counts; exit_code={exit}",
                format.name()
            );
        }
    }
    if let Some(limit) = context.limit_reached {
        let _ = writeln!(
            out,
            "run terminated early by {limit}; the transcript is incomplete and parsing was skipped — the partial capture is authoritative"
        );
    }
    let complete_inline = context.limit_reached.is_none()
        && context.source_unavailable_bytes_at_least == 0
        && safe_output.len() <= PROCESS_INLINE_RETENTION_BYTES;
    let silent_failure = context.exit_code != Some(0)
        && context.limit_reached.is_none()
        && summary.format != TestOutputFormat::Unknown
        && summary.failures.is_empty()
        && summary.counts.is_none_or(|counts| counts.failed == 0);
    if silent_failure {
        let _ = writeln!(
            out,
            "command exited with a failure outside the recognized test summary (exit_code={exit}); see the output below"
        );
    }
    if !summary.failures.is_empty() {
        let mut section = String::from("failing tests (verbatim):\n");
        for failure in summary.failures.iter().take(TEST_RUN_MAX_LISTED_FAILURES) {
            let _ = writeln!(section, "--- {} ---", failure.name);
            if failure.detail.is_empty() {
                section.push_str("(no per-test output captured; page the capture)\n");
            } else {
                section.push_str(&failure.detail);
                section.push('\n');
            }
        }
        if summary.failures.len() > TEST_RUN_MAX_LISTED_FAILURES {
            let _ = writeln!(
                section,
                "[... {} more failing tests; page the capture for the rest]",
                summary.failures.len() - TEST_RUN_MAX_LISTED_FAILURES
            );
        }
        match elide_text_head_tail(
            &section,
            TEST_RUN_FAILURE_DETAIL_MAX_BYTES,
            "test_run_failure_detail",
        ) {
            Some(elided) => out.push_str(&elided.text),
            None => out.push_str(&section),
        }
    } else if summary.counts.is_some_and(|counts| counts.failed > 0) {
        out.push_str(
            "failing-test details were not found in the recognized output sections; page the capture\n",
        );
    }
    if complete_inline {
        out.push_str("full output (complete):\n");
        out.push_str(safe_output);
        if !safe_output.ends_with('\n') {
            out.push('\n');
        }
        return out;
    }
    if summary.format == TestOutputFormat::Unknown || silent_failure {
        out.push_str("output tail:\n");
        match elide_text_head_tail(
            safe_output,
            TEST_RUN_UNKNOWN_TAIL_MAX_BYTES,
            "test_run_unknown_format_tail",
        ) {
            Some(elided) => out.push_str(&elided.text),
            None => out.push_str(safe_output),
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if context.capture_retained {
        out.push_str(&capture_paging_hint(
            context.call_id,
            context.output_bytes,
            context.source_unavailable_bytes_at_least,
        ));
        out.push('\n');
    }
    out
}
