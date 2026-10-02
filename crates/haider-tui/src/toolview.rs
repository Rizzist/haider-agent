//! Tool rows in the Claude Code idiom (973-tui-toolview) — the PURE half.
//!
//! Owner (2026-09-23): tool calls should read like Claude Code's rows
//! (`state/reference/claude-code-tool-rows-write-diff.png`):
//!
//! ```text
//!   ● Write(state/973-LANES.md)
//!     ⎿ Added 46 lines, removed 74 lines
//!        1 - # 973 lanes — planning (not started; 972 release chain first)
//!        1 + # 973 lanes — RE-SCOPED by owner 2026-09-23
//!       … +12 lines (⌃O to expand)
//! ```
//!
//! A human VERB with a short argument on the header, a `⎿` result summary,
//! an inline diff preview for writes and edits, and the full content one
//! gesture away. This module owns the vocabulary (verbs, path shortening,
//! one-line commands), the diff model, the `⎿` summary and the display rows
//! an expanded row or the full-detail view walks. Like [`crate::toolfold`]
//! it knows no colour: segments carry a [`Tone`], diff rows a [`DiffKind`],
//! and `render.rs`/`style.rs` turn both into ink.
//!
//! Nothing here truncates DATA: every builder takes the full arguments and
//! the full retained output, and only DISPLAY rows are ellipsized or
//! windowed. The full-detail view reads the same inputs unbounded.

use crate::toolfold::{Segment, Tone, first_meaningful_line, meaning_segments};
use haider_protocol::tool::EditSpanV1;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The `⎿` result elbow. Six cells, exactly the width of the elbow it
/// replaces, so the sub-line's text column stays under the verb.
pub const ELBOW: &str = "    ⎿ ";

/// Cells [`ELBOW`] costs.
pub const ELBOW_CELLS: usize = 6;

/// The settled-row bullet. Its TONE carries the outcome (ok / err / meta),
/// its SHAPE never changes — the reference's `●`.
pub const BULLET: &str = "●";

/// Diff rows a collapsed write/edit previews before yielding to the
/// `… +N lines` affordance. Small on purpose: the row is a summary.
pub const DIFF_PREVIEW_ROWS: usize = 4;

/// The expand affordance's key, named the way the rest of the TUI names
/// chords (`⌥T · ⌃O`).
pub const EXPAND_KEY: &str = "⌃O";

/// Indent of a diff / detail display row: aligned under the `⎿` text.
pub const DETAIL_INDENT: usize = 6;

/// A diff LCS table larger than this many cells falls back to a plain
/// removed-then-added listing (still complete, never truncated).
const LCS_MAX_CELLS: usize = 1_000_000;

// ------------------------------------------------------------ vocabulary ---

/// What family a tool belongs to — the key its verb, its `⎿` summary and
/// its fold phrase all hang off.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ToolKind {
    Read,
    Write,
    Edit,
    /// Content search and path globbing — both "search for a pattern".
    Search,
    Shell,
    Fetch,
    WebSearch,
    Other,
}

/// Classify a tool by name. Unknown tools are [`ToolKind::Other`] and keep
/// their own name as the verb.
#[must_use]
pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "fs_read" | "read" | "read_file" | "file_read" => ToolKind::Read,
        "fs_write" | "write" | "write_file" | "file_write" => ToolKind::Write,
        "fs_edit" | "edit" | "file_edit" | "str_replace" => ToolKind::Edit,
        "fs_search" | "grep" | "ripgrep" | "search" | "file_search" | "fs_glob" | "glob" => {
            ToolKind::Search
        }
        "bash" | "shell" | "sh" | "zsh" | "process_exec" | "ssh_shell" | "command" => {
            ToolKind::Shell
        }
        "web_fetch" | "fetch" => ToolKind::Fetch,
        "web_search" => ToolKind::WebSearch,
        _ => ToolKind::Other,
    }
}

/// The header verb: `Read`, `Write`, `Edit`, `Search`, `Glob`, `Bash`,
/// `Fetch`, `Web Search`, or the tool's own name.
#[must_use]
pub fn verb(name: &str) -> String {
    match tool_kind(name) {
        ToolKind::Read => "Read".to_owned(),
        ToolKind::Write => "Write".to_owned(),
        ToolKind::Edit => "Edit".to_owned(),
        ToolKind::Search if name.contains("glob") => "Glob".to_owned(),
        ToolKind::Search => "Search".to_owned(),
        ToolKind::Shell => "Bash".to_owned(),
        ToolKind::Fetch => "Fetch".to_owned(),
        ToolKind::WebSearch => "Web Search".to_owned(),
        ToolKind::Other => name.to_owned(),
    }
}

/// Where paths are shortened FROM: the session's workspace, then `~`.
/// Hashable because the transcript layout cache keys on it — a workspace
/// learned after rows were measured must re-shorten them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct PathContext {
    pub workspace: Option<String>,
    pub home: Option<String>,
}

impl PathContext {
    /// Shorten an absolute path for display: workspace-relative when it is
    /// inside the workspace, `~/…` when it is inside home, unchanged
    /// otherwise. Component-aware — `/Users/alice2` is not under `~alice`.
    #[must_use]
    pub fn shorten(&self, path: &str) -> String {
        if let Some(rest) = self
            .workspace
            .as_deref()
            .and_then(|root| strip_root(path, root))
        {
            return if rest.is_empty() {
                ".".to_owned()
            } else {
                rest.to_owned()
            };
        }
        if let Some(rest) = self.home.as_deref().and_then(|root| strip_root(path, root)) {
            return if rest.is_empty() {
                "~".to_owned()
            } else {
                format!("~/{rest}")
            };
        }
        path.to_owned()
    }

    /// A stable fingerprint for cache invalidation.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

fn strip_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return None;
    }
    let rest = path.strip_prefix(root)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/')
}

/// One display line from arbitrary text: the first non-blank line with its
/// whitespace runs collapsed, and ` …` when more non-blank lines follow — a
/// heredoc never spills its body onto a header.
#[must_use]
pub fn one_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines
        .next()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

fn arg_text<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(serde_json::Value::as_str))
        .filter(|text| !text.is_empty())
}

/// The FULL command a shell-ish call ran: `command`/`cmd`, or an `argv`
/// array joined with spaces.
#[must_use]
pub fn shell_command(args: &serde_json::Value) -> Option<String> {
    if let Some(command) = arg_text(args, &["command", "cmd", "script"]) {
        return Some(command.to_owned());
    }
    let argv = args.get("argv").and_then(serde_json::Value::as_array)?;
    let parts: Vec<&str> = argv.iter().filter_map(serde_json::Value::as_str).collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// The header's argument: a shortened path, a quoted pattern, a one-line
/// command, a URL — or the generic semantic summary for anything else.
/// Always ONE line; the renderer ellipsizes it to the row budget.
#[must_use]
pub fn header_args(name: &str, args: &serde_json::Value, paths: &PathContext) -> String {
    let path = arg_text(args, &["path", "file_path", "file"]).map(|path| paths.shorten(path));
    let fallback = || one_line(&crate::toolfold::semantic_summary(name, args));
    match tool_kind(name) {
        ToolKind::Read | ToolKind::Write | ToolKind::Edit => path.unwrap_or_else(fallback),
        ToolKind::Search => {
            let Some(pattern) = arg_text(args, &["pattern", "query", "search"]) else {
                return fallback();
            };
            let within = path.or_else(|| arg_text(args, &["root"]).map(|p| paths.shorten(p)));
            let glob = arg_text(args, &["glob", "include"]);
            let mut text = format!("\"{}\"", one_line(pattern));
            if let Some(within) = within {
                text.push_str(" in ");
                text.push_str(&within);
            }
            if let Some(glob) = glob {
                text.push(' ');
                text.push_str(glob);
            }
            text
        }
        ToolKind::Shell => shell_command(args).map_or_else(fallback, |command| one_line(&command)),
        ToolKind::Fetch => arg_text(args, &["url"]).map_or_else(fallback, one_line),
        ToolKind::WebSearch => arg_text(args, &["query"]).map_or_else(fallback, one_line),
        ToolKind::Other => fallback(),
    }
}

/// The FULL arguments for the detail view: the whole command for a shell
/// call (heredoc body included), otherwise pretty JSON. `None` for an empty
/// argument object.
#[must_use]
pub fn full_args(name: &str, args: &serde_json::Value) -> Option<String> {
    if tool_kind(name) == ToolKind::Shell
        && let Some(command) = shell_command(args)
    {
        return Some(command);
    }
    let empty = args.is_null() || args.as_object().is_some_and(serde_json::Map::is_empty);
    if empty {
        return None;
    }
    // A write's content and an edit's replacements ARE its diff, which the
    // view shows in full, wrapped and numbered: echoing them again as one
    // escaped JSON string would double a 120 KB write into an unreadable
    // wall. Only the fields the diff does not show stay here.
    if matches!(tool_kind(name), ToolKind::Write | ToolKind::Edit)
        && tool_diff(name, args, &[]).is_some()
        && let Some(object) = args.as_object()
    {
        let rest: serde_json::Map<String, serde_json::Value> = object
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "content" | "edits" | "old_string" | "new_string" | "old" | "new"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut text = serde_json::to_string_pretty(&serde_json::Value::Object(rest)).ok()?;
        text.push_str("\n(content: see the diff above)");
        return Some(text);
    }
    serde_json::to_string_pretty(args).ok()
}

/// What a run of foldable calls is called, keyed by the fold key the
/// transcript scan produced: `Read 3 files`, `Searched for 2 patterns`,
/// `Ran 3 shell commands`, `Fetched 2 URLs`.
///
/// Returns `(lead, noun)` around the emphasized count, so the fold row
/// keeps its single point of emphasis.
#[must_use]
pub fn fold_phrase(key: &str, count: usize) -> (String, String) {
    let plural = count != 1;
    let noun = |one: &str, many: &str| if plural { many } else { one }.to_owned();
    match key {
        "read" => ("Read ".to_owned(), noun("file", "files")),
        "search" => ("Searched for ".to_owned(), noun("pattern", "patterns")),
        "fetch" => ("Fetched ".to_owned(), noun("URL", "URLs")),
        "web_search" => ("Ran ".to_owned(), noun("web search", "web searches")),
        other => ("Ran ".to_owned(), crate::toolfold::fold_noun(other, count)),
    }
}

/// The fold key a settled, uneventful call contributes to a run scan.
/// Writes and edits NEVER fold — their diff is the point of the row.
#[must_use]
pub fn fold_key(name: &str) -> Option<String> {
    match tool_kind(name) {
        ToolKind::Write | ToolKind::Edit => None,
        ToolKind::Read => Some("read".to_owned()),
        ToolKind::Search => Some("search".to_owned()),
        // Model shell calls fold with model `$` commands: both are Bash.
        ToolKind::Shell => Some("command".to_owned()),
        ToolKind::Fetch => Some("fetch".to_owned()),
        ToolKind::WebSearch => Some("web_search".to_owned()),
        ToolKind::Other => Some(name.to_owned()),
    }
}

// ------------------------------------------------------------------ diff ---

/// One diff row's role.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
    /// The break between two edits of one call.
    Gap,
}

impl DiffKind {
    /// The one-character marker that carries the meaning without colour.
    #[must_use]
    pub const fn marker(self) -> char {
        match self {
            Self::Context => ' ',
            Self::Added => '+',
            Self::Removed => '-',
            Self::Gap => '⋯',
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiffLine {
    pub kind: DiffKind,
    /// The FILE line number when it is known (a whole-file write, or an
    /// edit whose location was resolved); `None` never guesses.
    pub number: Option<usize>,
    pub text: String,
}

/// The diff a write or edit carries in its own arguments.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ToolDiff {
    pub lines: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
    /// A whole-file write (`Wrote N lines`) rather than a replacement.
    pub wrote: bool,
}

impl ToolDiff {
    /// Width of the line-number gutter (0 when no row carries a number).
    #[must_use]
    pub fn gutter(&self) -> usize {
        self.lines
            .iter()
            .filter_map(|line| line.number)
            .max()
            .map_or(0, |max| max.to_string().len())
    }

    /// The rows a collapsed preview shows: from one context row before the
    /// first change, at most `rows` long. Returns `(start, end)`.
    #[must_use]
    pub fn preview_window(&self, rows: usize) -> (usize, usize) {
        let first_change = self
            .lines
            .iter()
            .position(|line| matches!(line.kind, DiffKind::Added | DiffKind::Removed))
            .unwrap_or(0);
        let start = first_change.saturating_sub(1);
        let start = if matches!(
            self.lines.get(start).map(|line| line.kind),
            Some(DiffKind::Context)
        ) || start == first_change
        {
            start
        } else {
            first_change
        };
        (start, start.saturating_add(rows).min(self.lines.len()))
    }
}

/// One edit's replacement pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditPair {
    pub old: String,
    pub new: String,
}

/// Every replacement an edit call carries: `edits[].{old,new}` (`fs_edit`),
/// or a single `old_string`/`new_string` (`edit`) or `old`/`new` pair.
#[must_use]
pub fn edit_pairs(args: &serde_json::Value) -> Vec<EditPair> {
    let pair = |value: &serde_json::Value, old: &str, new: &str| {
        let old = value.get(old).and_then(serde_json::Value::as_str)?;
        let new = value.get(new).and_then(serde_json::Value::as_str)?;
        Some(EditPair {
            old: old.to_owned(),
            new: new.to_owned(),
        })
    };
    if let Some(edits) = args.get("edits").and_then(serde_json::Value::as_array) {
        return edits
            .iter()
            .filter_map(|edit| pair(edit, "old", "new"))
            .collect();
    }
    pair(args, "old_string", "new_string")
        .or_else(|| pair(args, "old", "new"))
        .into_iter()
        .collect()
}

/// The diff a write/edit call carries.
///
/// An edit's rows are numbered ONLY from `spans` — the authoritative
/// coordinates the edit tool measured while applying each replacement
/// ([`EditSpanV1`]). A replacement with exactly one span is numbered from
/// it (removed rows in the pre-edit file, added rows in the post-edit file,
/// each side only when the tool proved it); a `replace_all` with several
/// occurrences, or a result without spans (an older journal, another tool),
/// shows no numbers. Nothing is inferred from text (973 repair 4).
///
/// A whole-file write numbers its rows from 1: they ARE the file's lines.
#[must_use]
pub fn tool_diff(name: &str, args: &serde_json::Value, spans: &[EditSpanV1]) -> Option<ToolDiff> {
    match tool_kind(name) {
        ToolKind::Write => {
            let content = args.get("content").and_then(serde_json::Value::as_str)?;
            let lines: Vec<DiffLine> = content
                .lines()
                .enumerate()
                .map(|(index, text)| DiffLine {
                    kind: DiffKind::Added,
                    number: Some(index + 1),
                    text: text.to_owned(),
                })
                .collect();
            Some(ToolDiff {
                added: lines.len(),
                removed: 0,
                lines,
                wrote: true,
            })
        }
        ToolKind::Edit => {
            let pairs = edit_pairs(args);
            if pairs.is_empty() {
                return None;
            }
            let mut diff = ToolDiff::default();
            for (index, pair) in pairs.iter().enumerate() {
                if index > 0 {
                    diff.lines.push(DiffLine {
                        kind: DiffKind::Gap,
                        number: None,
                        text: String::new(),
                    });
                }
                let (old_start, new_start) = span_starts(spans, index);
                for line in line_diff(&pair.old, &pair.new, old_start, new_start) {
                    match line.kind {
                        DiffKind::Added => diff.added += 1,
                        DiffKind::Removed => diff.removed += 1,
                        DiffKind::Context | DiffKind::Gap => {}
                    }
                    diff.lines.push(line);
                }
            }
            Some(diff)
        }
        _ => None,
    }
}

/// The pre-/post-edit start lines of edit `index`, when the tool measured
/// exactly one replacement for it.
fn span_starts(spans: &[EditSpanV1], index: usize) -> (Option<usize>, Option<usize>) {
    let mut own = spans
        .iter()
        .filter(|span| usize::try_from(span.edit_index).is_ok_and(|i| i == index));
    match (own.next(), own.next()) {
        (Some(span), None) => (
            span.old_start_line
                .and_then(|line| usize::try_from(line).ok()),
            span.new_start_line
                .and_then(|line| usize::try_from(line).ok()),
        ),
        _ => (None, None),
    }
}

/// A line-level diff of one replacement (longest common subsequence).
/// Removed rows number from `old_start` (the replaced text's first line in
/// the PRE-edit file), added rows from `new_start` (the inserted text's
/// first line in the POST-edit file); a context row is in both and takes
/// its post-edit number, else its pre-edit one. A missing start leaves
/// that side unnumbered.
#[must_use]
pub fn line_diff(
    old: &str,
    new: &str,
    old_start: Option<usize>,
    new_start: Option<usize>,
) -> Vec<DiffLine> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let old_number = |offset: usize| old_start.map(|start| start + offset);
    let new_number = |offset: usize| new_start.map(|start| start + offset);
    let mut out = Vec::with_capacity(old.len() + new.len());
    let cells = (old.len() + 1).saturating_mul(new.len() + 1);
    if cells > LCS_MAX_CELLS {
        out.extend(old.iter().enumerate().map(|(i, text)| DiffLine {
            kind: DiffKind::Removed,
            number: old_number(i),
            text: (*text).to_owned(),
        }));
        out.extend(new.iter().enumerate().map(|(j, text)| DiffLine {
            kind: DiffKind::Added,
            number: new_number(j),
            text: (*text).to_owned(),
        }));
        return out;
    }
    // table[i][j] = LCS length of old[i..] and new[j..].
    let width = new.len() + 1;
    let mut table = vec![0u32; cells];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            table[i * width + j] = if old[i] == new[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0usize, 0usize);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            out.push(DiffLine {
                kind: DiffKind::Context,
                number: new_number(j).or_else(|| old_number(i)),
                text: new[j].to_owned(),
            });
            i += 1;
            j += 1;
        } else if i < old.len()
            && (j == new.len() || table[(i + 1) * width + j] >= table[i * width + j + 1])
        {
            // On a tie the removal prints first: a replacement reads
            // old-then-new, the way every reader expects.
            out.push(DiffLine {
                kind: DiffKind::Removed,
                number: old_number(i),
                text: old[i].to_owned(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                kind: DiffKind::Added,
                number: new_number(j),
                text: new[j].to_owned(),
            });
            j += 1;
        }
    }
    out
}

// ------------------------------------------------------- result summary ---

/// How a call settled, as far as the `⎿` line is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Settle {
    #[default]
    Done,
    Running,
    Failed,
    Rejected,
    Conflict,
    Unknown,
    Cancelled,
}

impl Settle {
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Rejected | Self::Conflict | Self::Unknown
        )
    }
}

/// Everything the `⎿` summary needs about one call.
#[derive(Clone, Debug, Default)]
pub struct ResultFacts<'a> {
    pub kind: Option<ToolKind>,
    pub settle: Settle,
    pub exit_code: Option<i32>,
    pub reason: Option<&'a str>,
    /// The retained output (or the result preview when no output streamed).
    pub output: &'a str,
    /// The retained tail lost its front to the output cap.
    pub output_cut: bool,
    pub diff: Option<&'a ToolDiff>,
    /// Typed search match count from the result's structured data.
    pub matches: Option<usize>,
    /// The output is shown in full right below (an expanded row): a
    /// generic success then COUNTS it (`12 lines of output`) instead of
    /// quoting its first line twice.
    pub output_shown: bool,
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The phrase a FAILED call of this kind leads with.
#[must_use]
pub fn failure_phrase(kind: Option<ToolKind>, settle: Settle) -> &'static str {
    match settle {
        Settle::Rejected => return "Rejected",
        Settle::Conflict => return "Conflict",
        Settle::Unknown => return "Outcome unknown",
        _ => {}
    }
    match kind {
        Some(ToolKind::Write) => "Error writing file",
        Some(ToolKind::Edit) => "Error editing file",
        Some(ToolKind::Read) => "Error reading file",
        Some(ToolKind::Search) => "Search failed",
        Some(ToolKind::Shell) => "Command failed",
        Some(ToolKind::Fetch) => "Fetch failed",
        Some(ToolKind::WebSearch) => "Web search failed",
        Some(ToolKind::Other) | None => "Failed",
    }
}

/// The `⎿` result line. `None` while the call runs, and for a success with
/// nothing worth a line — the header then stands alone.
///
/// `width` is the full row width (0 disables budgeting); the line is
/// ellipsized to it, never wrapped.
#[must_use]
pub fn result_segments(facts: &ResultFacts<'_>, width: usize) -> Option<Vec<Segment>> {
    if facts.settle == Settle::Running {
        return None;
    }
    let body = if facts.output_cut {
        facts.output.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        facts.output
    };
    let total_lines = body.lines().count();
    let first = first_meaningful_line(body);
    let reason = facts.reason.map(one_line).filter(|text| !text.is_empty());
    let mut out: Vec<Segment> = Vec::new();
    let failing_exit = facts.exit_code.is_some_and(|code| code != 0);
    if facts.settle.is_failure() || failing_exit {
        let phrase = match facts.exit_code {
            Some(code) if code != 0 => format!("Exit code {code}"),
            _ => failure_phrase(facts.kind, facts.settle).to_owned(),
        };
        out.push(Segment::new(phrase, Tone::Err));
        if let Some(reason) = reason {
            out.push(Segment::new(format!(" · {reason}"), Tone::Err));
        } else if let Some(line) = first {
            out.push(Segment::new(" · ", Tone::Meta));
            out.extend(meaning_segments(line));
        }
    } else if facts.settle == Settle::Cancelled {
        out.push(Segment::new("Cancelled", Tone::Meta));
        if let Some(reason) = reason {
            out.push(Segment::new(format!(" · {reason}"), Tone::Meta));
        }
    } else {
        success_segments(facts, first, total_lines, &mut out);
        if out.is_empty() {
            return None;
        }
        if let Some(reason) = reason {
            // A reason on a settled success is a recovered retry: quiet.
            out.push(Segment::new(format!(" · {reason}"), Tone::Meta));
        }
    }
    let mut segments = vec![Segment::new(ELBOW, Tone::Structure)];
    segments.extend(if width == 0 {
        out
    } else {
        fit_segments(out, width.saturating_sub(ELBOW_CELLS))
    });
    Some(segments)
}

fn success_segments(
    facts: &ResultFacts<'_>,
    first: Option<&str>,
    total_lines: usize,
    out: &mut Vec<Segment>,
) {
    if let Some(diff) = facts.diff {
        if diff.wrote {
            out.push(Segment::new("Wrote ", Tone::Meta));
            out.push(Segment::new(diff.added.to_string(), Tone::Emphasis));
            out.push(Segment::new(
                if diff.added == 1 { " line" } else { " lines" },
                Tone::Meta,
            ));
            return;
        }
        let mut part = |verb: &str, count: usize| {
            out.push(Segment::new(verb.to_owned(), Tone::Meta));
            out.push(Segment::new(count.to_string(), Tone::Emphasis));
            out.push(Segment::new(
                if count == 1 { " line" } else { " lines" },
                Tone::Meta,
            ));
        };
        match (diff.added, diff.removed) {
            (0, 0) => out.push(Segment::new("No line changes", Tone::Meta)),
            (added, 0) => part("Added ", added),
            (0, removed) => part("Removed ", removed),
            (added, removed) => {
                part("Added ", added);
                part(", removed ", removed);
            }
        }
        return;
    }
    match facts.kind {
        Some(ToolKind::Read) if total_lines > 0 => {
            out.push(Segment::new("Read ", Tone::Meta));
            out.push(Segment::new(total_lines.to_string(), Tone::Emphasis));
            out.push(Segment::new(
                if total_lines == 1 { " line" } else { " lines" },
                Tone::Meta,
            ));
        }
        Some(ToolKind::Search) if facts.matches.is_some() => {
            let count = facts.matches.unwrap_or(0);
            out.push(Segment::new("Found ", Tone::Meta));
            out.push(Segment::new(count.to_string(), Tone::Emphasis));
            out.push(Segment::new(
                if count == 1 { " match" } else { " matches" },
                Tone::Meta,
            ));
        }
        Some(ToolKind::Shell) if first.is_none() => {
            out.push(Segment::new("(No output)", Tone::Meta));
        }
        _ if facts.output_shown => {
            if total_lines > 0 {
                out.push(Segment::new(
                    format!("{} of output", plural(total_lines, "line", "lines")),
                    Tone::Meta,
                ));
            }
        }
        _ => {
            if let Some(line) = first {
                out.extend(meaning_segments(line));
                if total_lines > 1 {
                    out.push(Segment::new(
                        format!(" … +{}", plural(total_lines - 1, "line", "lines")),
                        Tone::Meta,
                    ));
                }
            }
        }
    }
}

/// Fit toned segments into `budget` cells, ellipsizing the segment that
/// crosses the edge and dropping everything after it.
#[must_use]
pub fn fit_segments(segments: Vec<Segment>, budget: usize) -> Vec<Segment> {
    let mut used = 0usize;
    let mut out = Vec::with_capacity(segments.len());
    let total: usize = segments.iter().map(|s| s.text.width()).sum();
    if total <= budget {
        return segments;
    }
    for segment in segments {
        let cells = segment.text.width();
        if used + cells < budget {
            used += cells;
            out.push(segment);
            continue;
        }
        let room = budget.saturating_sub(used);
        if room > 0 {
            out.push(Segment::new(
                ellipsize_cells(&segment.text, room),
                segment.tone,
            ));
        }
        break;
    }
    out
}

/// Cell-aware ellipsize (wide glyphs count as two), trailing `…`.
#[must_use]
pub fn ellipsize_cells(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if text.width() <= budget {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let cells = grapheme.width();
        if used + cells > budget.saturating_sub(1) {
            break;
        }
        out.push_str(grapheme);
        used += cells;
    }
    out.push('…');
    out
}

/// The collapsed diff preview's closing affordance: what it hides and how
/// to open it.
#[must_use]
pub fn more_segments(hidden: usize) -> Vec<Segment> {
    vec![
        Segment::new(" ".repeat(DETAIL_INDENT), Tone::Structure),
        Segment::new(
            format!("… +{} ", plural(hidden, "line", "lines")),
            Tone::Meta,
        ),
        Segment::new(format!("({EXPAND_KEY} to expand)"), Tone::Meta),
    ]
}

/// The door for a preview that hid no LINES but clipped one: a one-line
/// write wider than the row still has more to show.
#[must_use]
pub fn clipped_segments() -> Vec<Segment> {
    vec![
        Segment::new(" ".repeat(DETAIL_INDENT), Tone::Structure),
        Segment::new("… line clipped ", Tone::Meta),
        Segment::new(format!("({EXPAND_KEY} to expand)"), Tone::Meta),
    ]
}

// --------------------------------------------------------- display rows ---

/// What one display row of an expanded tool row or the detail view is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowRole {
    /// A section label in the detail view (`Arguments`, `Output`, …).
    Heading,
    /// A line of the full arguments / command.
    Args,
    /// A diff row; `first` is false on a wrapped continuation.
    Diff { kind: DiffKind, first: bool },
    /// A line of retained output, colour-by-meaning.
    Output,
    /// Honesty / reason notes.
    Note(Tone),
}

/// One DISPLAY row: already wrapped to the content width.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DisplayRow {
    pub role: RowRole,
    /// The gutter text (line number, right-aligned) for a diff row.
    pub gutter: String,
    pub text: String,
}

/// Replace tabs and control characters so a row's cell width is what the
/// terminal will actually draw.
#[must_use]
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => out.push_str("    "),
            ch if ch.is_control() => out.push(' '),
            ch => out.push(ch),
        }
    }
    out
}

/// Hard-wrap one logical line to `width` cells at grapheme boundaries
/// (every suffix stays reachable). An empty line is one empty row.
#[must_use]
pub fn wrap_cells(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_owned()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cells = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme = if grapheme.width() > width {
            "�"
        } else {
            grapheme
        };
        let next = grapheme.width();
        if cells + next > width {
            rows.push(std::mem::take(&mut row));
            cells = 0;
        }
        row.push_str(grapheme);
        cells += next;
    }
    rows.push(row);
    rows
}

/// The diff's display rows at `width` total cells (indent + gutter +
/// marker included), wrapped so nothing is cut.
#[must_use]
pub fn diff_rows(diff: &ToolDiff, range: std::ops::Range<usize>, width: usize) -> Vec<DisplayRow> {
    let gutter = diff.gutter();
    // indent + gutter + " " + marker + " "
    let prefix = DETAIL_INDENT + gutter + if gutter > 0 { 1 } else { 0 } + 2;
    let text_width = width.saturating_sub(prefix).max(1);
    let mut rows = Vec::new();
    for line in diff.lines.get(range).unwrap_or_default() {
        let number = line
            .number
            .map(|n| format!("{n:>gutter$}"))
            .unwrap_or_else(|| " ".repeat(gutter));
        if line.kind == DiffKind::Gap {
            rows.push(DisplayRow {
                role: RowRole::Diff {
                    kind: DiffKind::Gap,
                    first: true,
                },
                gutter: " ".repeat(gutter),
                text: String::new(),
            });
            continue;
        }
        for (index, piece) in wrap_cells(&sanitize(&line.text), text_width)
            .into_iter()
            .enumerate()
        {
            rows.push(DisplayRow {
                role: RowRole::Diff {
                    kind: line.kind,
                    first: index == 0,
                },
                gutter: if index == 0 {
                    number.clone()
                } else {
                    " ".repeat(gutter)
                },
                text: piece,
            });
        }
    }
    rows
}

/// As [`diff_rows`], but ONE display row per diff line: a line wider than
/// the row is ellipsized. The collapsed preview is a glance, not a reader —
/// the full, wrapped rows live one gesture away.
#[must_use]
pub fn diff_rows_clipped(
    diff: &ToolDiff,
    range: std::ops::Range<usize>,
    width: usize,
) -> Vec<DisplayRow> {
    let gutter = diff.gutter();
    let prefix = DETAIL_INDENT + gutter + if gutter > 0 { 1 } else { 0 } + 2;
    let text_width = width.saturating_sub(prefix).max(1);
    diff.lines
        .get(range)
        .unwrap_or_default()
        .iter()
        .map(|line| DisplayRow {
            role: RowRole::Diff {
                kind: line.kind,
                first: true,
            },
            gutter: line
                .number
                .map(|n| format!("{n:>gutter$}"))
                .unwrap_or_else(|| " ".repeat(gutter)),
            text: if width == 0 {
                sanitize(&line.text)
            } else {
                ellipsize_cells(&sanitize(&line.text), text_width)
            },
        })
        .collect()
}

fn text_rows(text: &str, role: RowRole, width: usize, rows: &mut Vec<DisplayRow>) {
    let text_width = width.saturating_sub(DETAIL_INDENT).max(1);
    for line in text.lines() {
        for piece in wrap_cells(&sanitize(line), text_width) {
            rows.push(DisplayRow {
                role,
                gutter: String::new(),
                text: piece,
            });
        }
    }
}

/// Inputs to the expanded body of one tool row.
#[derive(Clone, Debug, Default)]
pub struct DetailFacts<'a> {
    /// Full arguments, shown when present (always in the detail view; in
    /// place only when the header could not carry them — a multi-line
    /// command).
    pub args: Option<&'a str>,
    pub diff: Option<&'a ToolDiff>,
    pub output: &'a str,
    pub output_truncated: bool,
    pub output_decode_error: bool,
    /// Honesty notes about the SOURCE of `output` (a capped stream replaced
    /// by the joined result; a result the tool declared bounded).
    pub source_notes: &'a [String],
    /// The FULL reason (the detail view never shortens it).
    pub reason: Option<&'a str>,
    /// Label each section (the detail view does; in place does not).
    pub headings: bool,
}

/// Every display row of an expanded tool row, wrapped to `width`.
#[must_use]
pub fn detail_rows(facts: &DetailFacts<'_>, width: usize) -> Vec<DisplayRow> {
    let mut rows = Vec::new();
    let heading = |rows: &mut Vec<DisplayRow>, text: String| {
        rows.push(DisplayRow {
            role: RowRole::Heading,
            gutter: String::new(),
            text,
        });
    };
    let args = facts.args.filter(|args| !args.is_empty());
    // A write's or edit's arguments ARE its diff: the diff leads and the
    // raw arguments close the view. Every other call leads with them.
    let args_last = facts.diff.is_some_and(|diff| !diff.lines.is_empty());
    let push_args = |rows: &mut Vec<DisplayRow>| {
        if let Some(args) = args {
            if facts.headings {
                heading(rows, "Arguments".to_owned());
            }
            text_rows(args, RowRole::Args, width, rows);
        }
    };
    if !args_last {
        push_args(&mut rows);
    }
    if let Some(diff) = facts.diff.filter(|diff| !diff.lines.is_empty()) {
        if facts.headings {
            heading(
                &mut rows,
                if diff.wrote {
                    format!("Content · {}", plural(diff.added, "line", "lines"))
                } else {
                    format!("Diff · +{} −{}", diff.added, diff.removed)
                },
            );
        }
        rows.extend(diff_rows(diff, 0..diff.lines.len(), width));
    }
    if !facts.output.is_empty() {
        if facts.headings {
            heading(
                &mut rows,
                format!(
                    "Output · {}",
                    plural(facts.output.lines().count(), "line", "lines")
                ),
            );
        }
        text_rows(facts.output, RowRole::Output, width, &mut rows);
    }
    if facts.output_truncated {
        text_rows(
            "⋯ output above is a bounded tail — earlier output truncated",
            RowRole::Note(Tone::Meta),
            width,
            &mut rows,
        );
    }
    if facts.output_decode_error {
        text_rows(
            "⚠ some output could not be decoded",
            RowRole::Note(Tone::Warn),
            width,
            &mut rows,
        );
    }
    for note in facts.source_notes {
        text_rows(note, RowRole::Note(Tone::Meta), width, &mut rows);
    }
    if facts.headings
        && let Some(reason) = facts.reason.filter(|reason| !reason.is_empty())
    {
        heading(&mut rows, "Reason".to_owned());
        text_rows(reason, RowRole::Note(Tone::Body), width, &mut rows);
    }
    if args_last {
        push_args(&mut rows);
    }
    rows
}
