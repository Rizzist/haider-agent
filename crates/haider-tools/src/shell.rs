//! Composer `!` escape parsing and harness-side shell builtins.
//!
//! Builtins mutate only [`ShellSession`] state and never spawn. Other escaped
//! commands become [`ProcessExec`] values which callers execute through
//! [`EffectBroker::process_exec_user`](crate::EffectBroker::process_exec_user),
//! preserving the distinct user-typed authorization source.

use crate::process::ProcessExec;
use crate::{ToolError, ToolResult};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

pub const REDACTED_ENV_VALUE: &str = "•redacted";
/// Maximum first-send bytes emitted by any deterministic output adapter.
pub const REDUCED_TOOL_OUTPUT_MAX_BYTES: usize = 8 * 1024;

/// Deterministic, first-send-only process output reducers. The raw transcript
/// remains the artifact authority; these values are prompt-facing diet facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputAdapter {
    RustCompiler,
    Test,
    GithubJson,
    Git,
    PackageManager,
    StackTrace,
    Directory,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReducedToolOutput {
    pub text: String,
    pub adapter: OutputAdapter,
    pub before_tokens: usize,
    pub after_tokens: usize,
}

/// Approximate provider token charge used by the fixture report. This is a
/// stable byte estimate, not a model-specific tokenizer invocation.
#[must_use]
pub const fn estimated_tokens(bytes: usize) -> usize {
    bytes.saturating_add(3) / 4
}

/// Detects and reduces bounded process output without a model round trip.
/// Reapplying it to the same bytes always produces byte-identical output.
#[must_use]
pub fn reduce_tool_output(tool: &str, output: &str, failed: bool) -> ReducedToolOutput {
    let stripped = strip_ansi(output);
    let adapter = detect_output_adapter(tool, &stripped);
    let reduced = match adapter {
        OutputAdapter::RustCompiler => reduce_rust_compiler(&stripped),
        OutputAdapter::Test => reduce_test_output(&stripped, failed),
        OutputAdapter::GithubJson => reduce_github_json(&stripped),
        OutputAdapter::Git => reduce_git_output(&stripped),
        OutputAdapter::PackageManager => reduce_package_output(&stripped),
        OutputAdapter::StackTrace => reduce_stack_trace(&stripped),
        OutputAdapter::Directory => reduce_directory_output(&stripped),
        OutputAdapter::Generic => reduce_generic_output(&stripped, failed),
    };
    let reduced = bound_reduced_output(&reduced, failed);
    ReducedToolOutput {
        text: reduced.clone(),
        adapter,
        before_tokens: estimated_tokens(output.len()),
        after_tokens: estimated_tokens(reduced.len()),
    }
}

fn detect_output_adapter(tool: &str, output: &str) -> OutputAdapter {
    let lower = output.to_ascii_lowercase();
    if lower.contains("error[")
        || lower.contains("could not compile")
        || (lower.contains("error:") && output.contains(" --> "))
    {
        OutputAdapter::RustCompiler
    } else if lower.contains("test result:")
        || lower.contains("failures:")
        || lower.lines().any(|line| line.ends_with(" ... ok"))
    {
        OutputAdapter::Test
    } else if (tool == "process_exec" || tool == "gh")
        && (lower.contains("\"node_id\"")
            || lower.contains("\"avatar_url\"")
            || (lower.contains("\"title\"") && lower.contains("\"url\"")))
    {
        OutputAdapter::GithubJson
    } else if output.contains("diff --git ")
        || output.lines().any(|line| line.starts_with("commit "))
    {
        OutputAdapter::Git
    } else if lower.contains("downloading ")
        || lower.contains("npm warn")
        || lower.contains("collecting ")
        || lower.contains("gradle") && lower.contains("build failed")
    {
        OutputAdapter::PackageManager
    } else if lower.contains("caused by:")
        || output
            .lines()
            .filter(|line| is_stack_frame(line))
            .take(3)
            .count()
            >= 3
    {
        OutputAdapter::StackTrace
    } else if output.lines().count() >= 40
        && output.lines().filter(|line| looks_like_path(line)).count() * 4
            >= output.lines().count() * 3
    {
        OutputAdapter::Directory
    } else {
        OutputAdapter::Generic
    }
}

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        match bytes.get(index).copied() {
            Some(b'[') => {
                index += 1;
                while let Some(byte) = bytes.get(index).copied() {
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            Some(b']') => {
                index += 1;
                while let Some(byte) = bytes.get(index).copied() {
                    index += 1;
                    if byte == 0x07 || (byte == 0x1b && bytes.get(index).copied() == Some(b'\\')) {
                        index += usize::from(byte == 0x1b);
                        break;
                    }
                }
            }
            Some(_) => index += 1,
            None => {}
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn reduce_rust_compiler(input: &str) -> String {
    let mut kept = Vec::new();
    let mut seen = HashSet::new();
    let mut keep_first_arrow = false;
    let mut arrow_kept = false;
    for line in input.lines() {
        let trimmed = line.trim_start();
        let plain_diagnostic = (trimmed.starts_with("error:")
            && !trimmed.starts_with("error: aborting")
            && !trimmed.contains("could not compile"))
            || trimmed.starts_with("warning:");
        if trimmed.starts_with("error[") || trimmed.starts_with("warning[") || plain_diagnostic {
            let new_diagnostic = seen.insert(trimmed.to_owned());
            keep_first_arrow = new_diagnostic;
            arrow_kept = false;
            if new_diagnostic {
                kept.push(line.to_owned());
            }
            continue;
        }
        if trimmed.starts_with("-->") && keep_first_arrow && !arrow_kept {
            kept.push(line.to_owned());
            arrow_kept = true;
            continue;
        }
        if keep_first_arrow && line.contains('|') && line.contains('^') {
            kept.push(line.to_owned());
            keep_first_arrow = false;
            continue;
        }
        if (trimmed.contains("could not compile")
            || trimmed.contains("error emitted")
            || trimmed.starts_with("error: aborting"))
            && seen.insert(trimmed.to_owned())
        {
            kept.push(line.to_owned());
        }
    }
    collapse_repeated_lines(&kept)
}

fn reduce_test_output(input: &str, failed: bool) -> String {
    let lines = input.lines().collect::<Vec<_>>();
    if failed {
        let failure_start = lines.iter().position(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("failures:")
                || lower.contains("panicked at")
                || lower.ends_with(" ... failed")
        });
        if let Some(failure_start) = failure_start {
            let mut retained = vec![false; lines.len()];
            for keep in retained.iter_mut().skip(failure_start).take(20) {
                *keep = true;
            }
            for (index, line) in lines.iter().enumerate() {
                let lower = line.to_ascii_lowercase();
                if lower.contains("panicked at")
                    || lower.contains("panic message")
                    || lower.contains("assertion failed")
                {
                    for keep in retained.iter_mut().skip(index).take(4) {
                        *keep = true;
                    }
                }
            }
            let final_tail_start = lines.len().saturating_sub(20).max(failure_start);
            for keep in retained.iter_mut().skip(final_tail_start) {
                *keep = true;
            }
            let body = join_output_lines(
                lines
                    .iter()
                    .zip(retained)
                    .filter(|(_, retain)| *retain)
                    .map(|(line, _)| (*line).to_owned())
                    .collect(),
            );
            let marker = "-- failures + final 20 lines (verbatim) --\n";
            if estimated_tokens(marker.len().saturating_add(body.len()))
                < estimated_tokens(input.len())
            {
                return format!("{marker}{body}");
            }
            return body;
        }
    }
    join_output_lines(
        lines
            .into_iter()
            .filter(|line| {
                let lower = line.to_ascii_lowercase();
                !lower.ends_with(" ... ok")
                    && (lower.contains("test result:")
                        || lower.ends_with(" ... failed")
                        || lower.starts_with("error:"))
            })
            .map(ToOwned::to_owned)
            .collect(),
    )
}

fn reduce_github_json(input: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return reduce_generic_output(input, false);
    };
    let rows = match value {
        serde_json::Value::Array(rows) => rows,
        row @ serde_json::Value::Object(_) => vec![row],
        _ => return reduce_generic_output(input, false),
    };
    let mut output = vec!["id\tnumber\ttitle\tstate\tauthor\turl".to_owned()];
    for row in rows {
        let serde_json::Value::Object(row) = row else {
            continue;
        };
        let field = |name: &str| row.get(name).map(json_scalar).unwrap_or_default();
        let author = row
            .get("author")
            .or_else(|| row.get("user"))
            .and_then(|value| match value {
                serde_json::Value::Object(author) => author
                    .get("login")
                    .or_else(|| author.get("name"))
                    .map(json_scalar),
                _ => Some(json_scalar(value)),
            })
            .unwrap_or_default();
        output.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            field("id"),
            field("number"),
            field("title"),
            field("state"),
            author,
            row.get("url")
                .or_else(|| row.get("html_url"))
                .map(json_scalar)
                .unwrap_or_default(),
        ));
    }
    join_output_lines(output)
}

fn json_scalar(value: &serde_json::Value) -> String {
    let scalar = match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        _ => String::new(),
    };
    scalar.replace(['\t', '\r', '\n'], " ")
}

fn reduce_git_output(input: &str) -> String {
    if !input.contains("diff --git ") && input.lines().any(|line| line.starts_with("commit ")) {
        let mut kept = Vec::new();
        for line in input.lines() {
            let trimmed = line.trim_start();
            if line.starts_with("commit ")
                || line.starts_with("Author:")
                || (!line.is_empty()
                    && line.starts_with("    ")
                    && !trimmed.starts_with("Signed-off-by:"))
            {
                kept.push(line.to_owned());
            }
        }
        return collapse_repeated_lines(&kept);
    }
    let lines = input.lines().collect::<Vec<_>>();
    let mut keep = vec![false; lines.len()];
    let mut changed_per_file = 0usize;
    let mut capped_files = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") || line.starts_with("commit ") {
            changed_per_file = 0;
            keep[index] = true;
            continue;
        }
        if line.starts_with("@@") || line.starts_with("--- ") || line.starts_with("+++ ") {
            keep[index] = true;
        }
        if (line.starts_with('+') && !line.starts_with("+++"))
            || (line.starts_with('-') && !line.starts_with("---"))
        {
            if changed_per_file < 200 {
                let start = index.saturating_sub(3);
                let end = index.saturating_add(3).min(lines.len().saturating_sub(1));
                keep[start..=end].fill(true);
            }
            changed_per_file = changed_per_file.saturating_add(1);
            if changed_per_file == 201 {
                capped_files = capped_files.saturating_add(1);
            }
        }
    }
    let mut output = lines
        .iter()
        .zip(keep)
        .filter(|(_, keep)| *keep)
        .map(|(line, _)| (*line).to_owned())
        .collect::<Vec<_>>();
    if capped_files > 0 {
        output.push(format!(
            "[… diff changes capped after 200 lines in {capped_files} file(s) …]"
        ));
    }
    join_output_lines(output)
}

fn reduce_package_output(input: &str) -> String {
    let mut kept = Vec::new();
    let mut error_tail = 0usize;
    for line in input.lines() {
        let lower = line.to_ascii_lowercase();
        let error =
            lower.contains("error") || lower.contains("failed") || lower.contains("failure");
        let warning =
            lower.contains("warning") || lower.split_ascii_whitespace().any(|word| word == "warn");
        let warning_with_anchor = warning && (line.contains(':') || lower.contains(" at "));
        if error || warning_with_anchor {
            kept.push(line.to_owned());
            error_tail = usize::from(error) * 8;
            continue;
        }
        if error_tail > 0
            && !lower.contains("downloading")
            && !lower.contains("progress")
            && !line.trim().is_empty()
        {
            kept.push(line.to_owned());
            error_tail = error_tail.saturating_sub(1);
        }
    }
    collapse_repeated_lines(&kept)
}

fn reduce_stack_trace(input: &str) -> String {
    let mut frames = 0usize;
    let kept = input
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.contains("caused by:")
                || lower.contains("exception")
                || lower.contains("error")
            {
                return true;
            }
            if is_stack_frame(line) && frames < 12 {
                frames = frames.saturating_add(1);
                return true;
            }
            false
        })
        .map(ToOwned::to_owned)
        .collect();
    join_output_lines(kept)
}

fn is_stack_frame(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("at ")
        || trimmed.starts_with("File \"")
        || trimmed
            .split_once(':')
            .is_some_and(|(prefix, _)| prefix.trim().chars().all(|value| value.is_ascii_digit()))
}

fn reduce_directory_output(input: &str) -> String {
    let mut extensions = HashMap::<String, usize>::new();
    let mut vendor = HashMap::<String, usize>::new();
    let mut directories = BTreeMap::<String, Vec<String>>::new();
    for line in input.lines() {
        let normalized = line.replace('\\', "/");
        let lower = normalized.to_ascii_lowercase();
        if let Some(name) = ["node_modules", "target", "vendor", ".venv"]
            .into_iter()
            .find(|name| lower.split(['/', '\\']).any(|part| part == *name))
        {
            *vendor.entry(name.to_owned()).or_insert(0) += 1;
            continue;
        }
        let extension = Path::new(&normalized)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("[none]")
            .to_ascii_lowercase();
        *extensions.entry(extension).or_insert(0) += 1;
        let (directory, name) = normalized
            .rsplit_once('/')
            .map_or((".", normalized.as_str()), |(directory, name)| {
                (directory, name)
            });
        directories
            .entry(directory.to_owned())
            .or_default()
            .push(name.to_owned());
    }
    let mut first = Vec::new();
    for (directory, mut entries) in directories {
        entries.sort();
        entries.dedup();
        first.push(format!("{directory}/ ({})", entries.len()));
        first.extend(entries.iter().take(8).map(|name| format!("  {name}")));
        if entries.len() > 8 {
            first.push(format!("  [… {} more …]", entries.len().saturating_sub(8)));
        }
    }
    first.push("-- counts --".to_owned());
    let mut counts = extensions.into_iter().collect::<Vec<_>>();
    counts.sort_by(|left, right| left.0.cmp(&right.0));
    first.extend(
        counts
            .into_iter()
            .map(|(extension, count)| format!(".{extension}: {count}")),
    );
    let mut vendor = vendor.into_iter().collect::<Vec<_>>();
    vendor.sort_by(|left, right| left.0.cmp(&right.0));
    first.extend(
        vendor
            .into_iter()
            .map(|(name, count)| format!("{name}/: {count} paths collapsed")),
    );
    join_output_lines(first)
}

fn bound_reduced_output(input: &str, failed: bool) -> String {
    if input.len() <= REDUCED_TOOL_OUTPUT_MAX_BYTES {
        return input.to_owned();
    }
    let marker = "\n[… output adapter byte cap reached; raw transcript in artifact …]\n";
    if failed {
        let tail_budget = REDUCED_TOOL_OUTPUT_MAX_BYTES / 2;
        let tail_start = utf8_suffix_start(input, tail_budget);
        let prefix_budget = REDUCED_TOOL_OUTPUT_MAX_BYTES
            .saturating_sub(input.len().saturating_sub(tail_start))
            .saturating_sub(marker.len());
        let mut output = utf8_prefix(input, prefix_budget).to_owned();
        output.push_str(marker);
        output.push_str(&input[tail_start..]);
        return output;
    }
    let prefix_budget = REDUCED_TOOL_OUTPUT_MAX_BYTES.saturating_sub(marker.len());
    let mut output = utf8_prefix(input, prefix_budget).to_owned();
    output.push_str(marker);
    output
}

fn utf8_prefix(input: &str, max_bytes: usize) -> &str {
    let mut end = input.len().min(max_bytes);
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn utf8_suffix_start(input: &str, max_bytes: usize) -> usize {
    let mut start = input.len().saturating_sub(max_bytes);
    while start < input.len() && !input.is_char_boundary(start) {
        start += 1;
    }
    start
}

fn looks_like_path(line: &str) -> bool {
    line.matches(':').count() < 2
        && !line.contains(char::is_whitespace)
        && (line.contains('/') || Path::new(line).extension().is_some())
}

fn reduce_generic_output(input: &str, failed: bool) -> String {
    let lines = input.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    if !failed || lines.len() <= 20 {
        return collapse_repeated_lines(&lines);
    }
    let split = lines.len().saturating_sub(20);
    let mut reduced = collapse_repeated_lines(&lines[..split]);
    if !reduced.is_empty() && !reduced.ends_with('\n') {
        reduced.push('\n');
    }
    reduced.push_str("-- failure tail (verbatim) --\n");
    reduced.push_str(&join_output_lines(lines[split..].to_vec()));
    reduced
}

fn collapse_repeated_lines(lines: &[String]) -> String {
    let mut output = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let mut end = index.saturating_add(1);
        while end < lines.len() && lines[end] == lines[index] {
            end += 1;
        }
        let count = end.saturating_sub(index);
        if count > 1 {
            output.push(format!("{} [repeated {count}×]", lines[index]));
        } else {
            output.push(lines[index].clone());
        }
        index = end;
    }
    join_output_lines(output)
}

fn join_output_lines(lines: Vec<String>) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        let mut output = lines.join("\n");
        output.push('\n');
        output
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerSubmission {
    Message(String),
    Builtin(BuiltinResult),
    UserProcess(UserProcessExec),
}

/// A process command carrying unforgeable direct-composer provenance.
///
/// Public callers may receive this value from [`ShellSession::submit`], but
/// cannot construct one or turn a model-created [`ProcessExec`] into one.
///
/// ```compile_fail
/// use haider_tools::{ProcessExec, UserProcessExec};
///
/// let forged = UserProcessExec {
///     operation: ProcessExec::new("model-call", "echo forged"),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserProcessExec {
    operation: ProcessExec,
    provenance: UserTypedProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UserTypedProvenance(());

impl UserProcessExec {
    fn new(operation: ProcessExec) -> Self {
        Self {
            operation,
            provenance: UserTypedProvenance(()),
        }
    }

    pub(crate) fn operation(&self) -> &ProcessExec {
        &self.operation
    }

    pub(crate) fn provenance(&self) -> UserTypedProvenance {
        self.provenance
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinResult {
    ChangedDirectory { cwd: PathBuf },
    Environment { entries: Vec<EnvViewEntry> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvViewEntry {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShellSession {
    workspace_root: PathBuf,
    cwd: PathBuf,
    env_allowlist: Vec<String>,
    next_call: u64,
}

impl ShellSession {
    pub fn new(workspace_root: impl AsRef<Path>, env_allowlist: Vec<String>) -> ToolResult<Self> {
        let requested = workspace_root.as_ref();
        let workspace_root = std::fs::canonicalize(requested)
            .map_err(|error| ToolError::io("canonicalize shell workspace", requested, error))?;
        if !workspace_root.is_dir() {
            return Err(ToolError::invalid_argument(format!(
                "shell workspace is not a directory: {}",
                workspace_root.display()
            )));
        }
        let mut env_allowlist = env_allowlist;
        env_allowlist.sort();
        env_allowlist.dedup();
        if env_allowlist.iter().any(|name| name.is_empty()) {
            return Err(ToolError::invalid_argument(
                "shell env_allowlist names must not be empty",
            ));
        }
        Ok(Self {
            cwd: workspace_root.clone(),
            workspace_root,
            env_allowlist,
            next_call: 0,
        })
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Prepares one daemon-received user command without trimming or
    /// re-quoting its shell program. The caller-provided id becomes the
    /// process call id, which binds output/item receipts to the durable RPC
    /// command id. An optional cwd is workspace-relative and applies only to
    /// this invocation; it does not mutate shell or session state.
    pub fn prepare_user_process(
        &self,
        call_id: impl Into<String>,
        command: impl Into<String>,
        cwd: Option<&Path>,
    ) -> ToolResult<UserProcessExec> {
        let call_id = call_id.into();
        let command = command.into();
        if call_id.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "direct shell command id must not be empty",
            ));
        }
        if command.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "direct shell command must not be empty",
            ));
        }
        let cwd = match cwd {
            Some(cwd) => {
                if cwd.as_os_str().is_empty() || cwd.is_absolute() {
                    return Err(ToolError::invalid_argument(
                        "direct shell cwd must be a non-empty workspace-relative path",
                    ));
                }
                self.workspace_root.join(cwd)
            }
            None => self.cwd.clone(),
        };
        Ok(UserProcessExec::new(
            ProcessExec::new(call_id, command)
                .with_cwd(cwd)
                .with_env_allowlist(self.env_allowlist.clone()),
        ))
    }

    pub fn submit(&mut self, text: impl Into<String>) -> ToolResult<ComposerSubmission> {
        let text = text.into();
        let Some(escaped) = text.strip_prefix('!') else {
            return Ok(ComposerSubmission::Message(text));
        };
        let command = escaped.trim();
        if command.is_empty() {
            return Err(ToolError::invalid_argument(
                "shell escape requires a command after `!`",
            ));
        }
        if command == "cd" {
            return self.change_directory_to(self.workspace_root.clone(), ".");
        }
        if let Some(path) = command.strip_prefix("cd ") {
            return self.change_directory(path.trim());
        }
        if command == "env-view" {
            return Ok(ComposerSubmission::Builtin(BuiltinResult::Environment {
                entries: self
                    .env_allowlist
                    .iter()
                    .map(|name| EnvViewEntry {
                        name: name.clone(),
                        value: display_env_value(name, env::var(name).ok()),
                    })
                    .collect(),
            }));
        }

        self.next_call += 1;
        Ok(ComposerSubmission::UserProcess(UserProcessExec::new(
            ProcessExec::new(format!("shell-{}", self.next_call), command)
                .with_cwd(self.cwd.clone())
                .with_env_allowlist(self.env_allowlist.clone()),
        )))
    }

    fn change_directory(&mut self, path: &str) -> ToolResult<ComposerSubmission> {
        let path_buf = PathBuf::from(path);
        let requested = if path_buf.is_absolute() {
            path_buf
        } else {
            self.cwd.join(path_buf)
        };
        self.change_directory_to(requested, path)
    }

    fn change_directory_to(
        &mut self,
        requested: PathBuf,
        display_path: &str,
    ) -> ToolResult<ComposerSubmission> {
        let resolved = std::fs::canonicalize(&requested)
            .map_err(|error| ToolError::io("change shell directory", &requested, error))?;
        if !resolved.starts_with(&self.workspace_root) {
            return Err(ToolError::WorkspaceBoundary {
                workspace_root: self.workspace_root.clone(),
                requested_path: PathBuf::from(display_path),
                resolved_path: Some(resolved),
            });
        }
        if !resolved.is_dir() {
            return Err(ToolError::invalid_argument(format!(
                "shell cwd is not a directory: {}",
                resolved.display()
            )));
        }
        self.cwd = resolved.clone();
        Ok(ComposerSubmission::Builtin(
            BuiltinResult::ChangedDirectory { cwd: resolved },
        ))
    }
}

fn is_secret_env_name(name: &str) -> bool {
    const KNOWN_SECRET_NAMES: &[&str] = &[
        "PGPASSWORD",
        "MYSQL_PWD",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "NPM_TOKEN",
    ];
    const SECRET_SUBSTRINGS: &[&str] = &["PASSWORD", "PASSWD", "PWD", "PASSPHRASE"];
    const SECRET_WORDS: &[&str] = &[
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "CREDENTIALS",
        "BEARER",
    ];
    let uppercase = name.to_ascii_uppercase();
    KNOWN_SECRET_NAMES.contains(&uppercase.as_str())
        || SECRET_SUBSTRINGS
            .iter()
            .any(|secret| uppercase.contains(secret))
        || uppercase
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|word| SECRET_WORDS.contains(&word))
}

fn display_env_value(name: &str, value: Option<String>) -> Option<String> {
    value.map(|value| {
        if is_secret_env_name(name) {
            REDACTED_ENV_VALUE.to_owned()
        } else {
            value
        }
    })
}
