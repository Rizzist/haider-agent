//! W-C M1 — custom slash commands loaded from Markdown files.
//!
//! Two sources, both CLAUDE-CODE-COMPATIBLE so users can drop their existing
//! `.claude/commands/*.md` files in unchanged (renamed to `.haider/`):
//!
//! - project: the nearest `.haider/commands/` directory, discovered by
//!   walking UP from the workspace cwd (like project instructions);
//! - global:  `~/.haider/commands/`.
//!
//! Project wins on a name collision; both merge OVER the built-in registry
//! (they never replace it). A subdirectory is a namespace
//! (`.haider/commands/git/commit.md` → `/git:commit`, the CC convention).
//!
//! The YAML frontmatter keys honored are `description`, `argument-hint`,
//! `model` (an optional per-command pair override), and `allowed-tools`
//! (PARSED and stored but NOT enforced this wave). The body is the prompt
//! template. Expansion is TEXTUAL: `$ARGUMENTS` (all args, space-joined),
//! `$1`..`$N` (positional), and `$ARGUMENTS[N]` (an alias for `$N`); a `$`
//! not followed by a recognized token is literal. There is NO inline shell
//! execution — a custom command expands to a PROMPT ONLY (a user turn with
//! the substituted body).
//!
//! Loading is CLIENT-side (the command registry authority lives in the TUI).
//! A malformed file is SKIPPED with a surfaced warning — never a crash.

use std::path::{Path, PathBuf};

/// Where a loaded command came from — project wins collisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Project,
    Global,
}

impl CommandSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

/// One custom command, ready to merge into the palette and be expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// Slash name WITHOUT the leading `/`, lowercased; subdirectories become
    /// `:`-separated namespaces (`git/commit.md` → `git:commit`).
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// Optional per-command model/pair override. Validated against the
    /// discovered catalog at expansion time (unknown → ignored-with-note).
    pub model: Option<String>,
    /// PARSED and stored, NOT enforced this wave (enforcement is a follow-up).
    pub allowed_tools: Vec<String>,
    /// The prompt template (frontmatter stripped).
    pub body: String,
    pub source: CommandSource,
}

impl CustomCommand {
    /// The palette description column: the frontmatter `description`, or a
    /// fallback naming the source so an unlabeled command is still legible.
    #[must_use]
    pub fn palette_desc(&self) -> String {
        match &self.description {
            Some(desc) if !desc.is_empty() => desc.clone(),
            _ => format!("{} command", self.source.label()),
        }
    }

    /// Expand the template against a raw argument string (whitespace-split).
    #[must_use]
    pub fn expand(&self, arg_str: &str) -> Expansion {
        let args: Vec<&str> = arg_str.split_whitespace().collect();
        Expansion {
            body: substitute(&self.body, &args),
            model: self.model.clone(),
        }
    }
}

/// The result of expanding a custom command: the prompt body and the
/// optional per-command model/pair override to apply before the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub body: String,
    pub model: Option<String>,
}

/// A parsed command file, before it is named/sourced by the loader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommandFile {
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub body: String,
}

/// Why a file could not be parsed (always skipped, never fatal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A `---` frontmatter fence was opened but never closed.
    UnterminatedFrontmatter,
    /// A non-blank frontmatter line was not a `key: value` pair (M5): the
    /// frontmatter is malformed, so the whole file is skipped with a warning
    /// rather than silently accepting a half-parsed header.
    MalformedFrontmatter,
    /// The prompt body was empty (nothing to run).
    EmptyBody,
}

impl ParseError {
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::UnterminatedFrontmatter => "unterminated `---` frontmatter",
            Self::MalformedFrontmatter => "malformed frontmatter (expected `key: value`)",
            Self::EmptyBody => "empty command body",
        }
    }
}

/// The outcome of a load pass: the merged commands plus any skip warnings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadResult {
    pub commands: Vec<CustomCommand>,
    pub warnings: Vec<String>,
}

/// Bound on the ancestors the project walk-up visits.
const MAX_ANCESTORS: usize = 256;
/// Bound on the directory recursion depth per source (namespacing depth).
const MAX_NAMESPACE_DEPTH: usize = 16;
/// Bound on the number of command files loaded per source.
const MAX_FILES_PER_SOURCE: usize = 512;
/// M8: hard cap on directory entries the walk will EXAMINE per source. Applied
/// during the walk (not after collecting everything), so a directory holding
/// millions of entries can never stall or OOM TUI startup.
const MAX_WALK_ENTRIES: usize = 20_000;
/// M8: per-file byte cap — a command file is a small prompt template; a giant
/// `.md` is skipped with a warning rather than read whole into memory.
const MAX_COMMAND_FILE_BYTES: u64 = 1024 * 1024;

/// Parse one command file's contents into frontmatter + body.
///
/// A file with no `---` fence is all body. Unknown frontmatter keys are
/// tolerated (CC compat). Two shapes are malformed and rejected: an
/// unterminated frontmatter fence, and an empty body.
pub fn parse_command_file(contents: &str) -> Result<ParsedCommandFile, ParseError> {
    let mut description = None;
    let mut argument_hint = None;
    let mut model = None;
    let mut allowed_tools = Vec::new();

    // The frontmatter fence must be the very first line (CC convention).
    // M6: walk lines WITH their terminators (`split_inclusive`) so the body
    // byte offset is exact for BOTH `\n` and `\r\n`. The old `line.len() + 1`
    // undercounted every CRLF ending by one byte, which bled part of the
    // closing `---` fence into the prompt.
    let body = if matches!(contents.lines().next().map(str::trim), Some("---")) {
        let mut pieces = contents.split_inclusive('\n');
        // Consume the opening fence line; `opening` is its exact byte length.
        let opening = pieces.next().map_or(0, str::len);
        let mut closed = false;
        let mut malformed = false;
        let mut consumed = 0usize;
        for piece in pieces.by_ref() {
            consumed += piece.len();
            let line = piece.trim_end_matches(['\n', '\r']);
            if line.trim() == "---" {
                closed = true;
                break;
            }
            if line.trim().is_empty() {
                // A blank line inside frontmatter is fine.
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                // M5: a non-blank, non `key: value` line is MALFORMED. The
                // brief requires skip-with-warning, not the old silent accept.
                // Remember it but keep scanning: a fence that never closes is
                // reported as UNTERMINATED (that verdict wins) so the existing
                // unterminated-frontmatter law is preserved.
                malformed = true;
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = unquote(value.trim());
            match key.as_str() {
                "description" => description = non_empty(value),
                "argument-hint" => argument_hint = non_empty(value),
                "model" => model = non_empty(value),
                "allowed-tools" => allowed_tools = parse_tool_list(value),
                _ => {}
            }
        }
        if !closed {
            return Err(ParseError::UnterminatedFrontmatter);
        }
        if malformed {
            return Err(ParseError::MalformedFrontmatter);
        }
        // The body is everything after the closing fence.
        contents
            .get(opening + consumed..)
            .unwrap_or("")
            .trim_start_matches(['\n', '\r'])
            .to_owned()
    } else {
        contents.to_owned()
    };

    if body.trim().is_empty() {
        return Err(ParseError::EmptyBody);
    }
    Ok(ParsedCommandFile {
        description,
        argument_hint,
        model,
        allowed_tools,
        body,
    })
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// Strip one layer of surrounding single or double quotes.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if value.len() >= 2
        && ((bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

/// Parse an `allowed-tools` value: `[a, b]` or `a, b`, comma-separated.
fn parse_tool_list(value: String) -> Vec<String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(&value);
    inner
        .split(',')
        .map(|tool| unquote(tool.trim()))
        .filter(|tool| !tool.is_empty())
        .collect()
}

/// Textual argument substitution (see the module docs for the grammar).
#[must_use]
pub fn substitute(template: &str, args: &[&str]) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // `$ARGUMENTS[N]` — the alias for `$N`, tried first (longest match).
        if let Some((index, next)) = match_arguments_indexed(&chars, i + 1) {
            push_positional(&mut out, args, index);
            i = next;
            continue;
        }
        // `$ARGUMENTS` — all args, space-joined.
        if let Some(next) = match_word(&chars, i + 1, "ARGUMENTS") {
            out.push_str(&args.join(" "));
            i = next;
            continue;
        }
        // `$N` — a positional (1-based); unfilled expands to empty.
        if let Some((index, next)) = match_digits(&chars, i + 1) {
            push_positional(&mut out, args, index);
            i = next;
            continue;
        }
        // A bare `$` is literal.
        out.push('$');
        i += 1;
    }
    out
}

fn push_positional(out: &mut String, args: &[&str], one_based: usize) {
    if one_based >= 1
        && let Some(arg) = args.get(one_based - 1)
    {
        out.push_str(arg);
    }
}

/// Match the literal `word` at `start`; returns the index just past it.
fn match_word(chars: &[char], start: usize, word: &str) -> Option<usize> {
    let mut i = start;
    for wc in word.chars() {
        if chars.get(i) != Some(&wc) {
            return None;
        }
        i += 1;
    }
    Some(i)
}

/// Match a run of ASCII digits at `start`; returns (value, index-past).
fn match_digits(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let mut i = start;
    let mut value = 0usize;
    while let Some(c) = chars.get(i).filter(|c| c.is_ascii_digit()) {
        value = value
            .saturating_mul(10)
            .saturating_add((*c as usize) - ('0' as usize));
        i += 1;
    }
    (i > start).then_some((value, i))
}

/// Match `ARGUMENTS[<digits>]` at `start`; returns (N, index-past).
fn match_arguments_indexed(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let after_word = match_word(chars, start, "ARGUMENTS")?;
    if chars.get(after_word) != Some(&'[') {
        return None;
    }
    let (value, after_digits) = match_digits(chars, after_word + 1)?;
    if chars.get(after_digits) != Some(&']') {
        return None;
    }
    Some((value, after_digits + 1))
}

/// The nearest ancestor `.haider/commands` directory, from `start` upward.
///
/// M7: the discovered directory must stay CONTAINED within its project root —
/// an untrusted checkout could make `.haider/commands` a symlink pointing
/// outside the repo (at `/etc`, a huge tree, …) to redirect command loading.
/// We canonicalize the candidate and require it to live under the (canonical)
/// ancestor it was found in; an escaping symlink is skipped, and the walk
/// continues upward rather than loading through it.
#[must_use]
pub fn discover_project_commands_dir(start: &Path) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(start);
    for _ in 0..MAX_ANCESTORS {
        let dir = current?;
        let candidate = dir.join(".haider").join("commands");
        if candidate.is_dir() && path_is_contained(&candidate, dir) {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

/// M7: whether `candidate`'s CANONICAL target stays under `root`'s canonical
/// path — i.e. following every symlink in `candidate` never escapes the project
/// tree it was found in. A canonicalization failure (a race, a broken link) is
/// treated as NOT contained (fail closed).
fn path_is_contained(candidate: &Path, root: &Path) -> bool {
    match (candidate.canonicalize(), root.canonicalize()) {
        (Ok(real_candidate), Ok(real_root)) => real_candidate.starts_with(&real_root),
        _ => false,
    }
}

/// The global `~/.haider/commands` directory (not required to exist).
#[must_use]
pub fn global_commands_dir(home: &Path) -> PathBuf {
    home.join(".haider").join("commands")
}

/// Load and merge the project + global command sources.
///
/// `project_dir`/`global_dir` are the two `commands` directories (either may
/// be absent). Project wins collisions; both merge over the built-ins.
#[must_use]
pub fn load(project_dir: Option<&Path>, global_dir: Option<&Path>) -> LoadResult {
    use std::collections::BTreeMap;
    let mut merged: BTreeMap<String, CustomCommand> = BTreeMap::new();
    let mut warnings = Vec::new();

    // Global first, then project — project overwrites on name collision.
    if let Some(dir) = global_dir {
        load_dir(dir, CommandSource::Global, &mut merged, &mut warnings);
    }
    if let Some(dir) = project_dir {
        load_dir(dir, CommandSource::Project, &mut merged, &mut warnings);
    }
    LoadResult {
        commands: merged.into_values().collect(),
        warnings,
    }
}

/// Convenience: discover both dirs from `cwd`/`home` and load.
#[must_use]
pub fn load_for(cwd: Option<&Path>, home: Option<&Path>) -> LoadResult {
    let project = cwd.and_then(discover_project_commands_dir);
    let global = home.map(global_commands_dir);
    load(project.as_deref(), global.as_deref())
}

fn load_dir(
    root: &Path,
    source: CommandSource,
    merged: &mut std::collections::BTreeMap<String, CustomCommand>,
    warnings: &mut Vec<String>,
) {
    let mut files = Vec::new();
    // M8: bound the WALK (entries examined + files collected) so a directory
    // with millions of files can't stall/OOM startup — the cap must apply
    // DURING the walk, not after collecting everything.
    let mut entry_budget = MAX_WALK_ENTRIES;
    collect_md_files(root, root, 0, &mut files, &mut entry_budget);
    files.sort();
    for (name, path) in files.into_iter().take(MAX_FILES_PER_SOURCE) {
        // M8: skip a giant file rather than read it whole into memory.
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.len() > MAX_COMMAND_FILE_BYTES
        {
            warnings.push(format!(
                "{}: skipped (over {MAX_COMMAND_FILE_BYTES}-byte command-file cap)",
                path.display()
            ));
            continue;
        }
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                warnings.push(format!("{}: unreadable ({error})", path.display()));
                continue;
            }
        };
        match parse_command_file(&contents) {
            Ok(parsed) => {
                merged.insert(
                    name.clone(),
                    CustomCommand {
                        name,
                        description: parsed.description,
                        argument_hint: parsed.argument_hint,
                        model: parsed.model,
                        allowed_tools: parsed.allowed_tools,
                        body: parsed.body,
                        source,
                    },
                );
            }
            Err(error) => {
                warnings.push(format!("{}: {}", path.display(), error.reason()));
            }
        }
    }
}

/// Recursively collect `(namespaced-name, path)` for every `.md` file under
/// `root`, using the path relative to `root` for the `:` namespace.
fn collect_md_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
    entry_budget: &mut usize,
) {
    if depth > MAX_NAMESPACE_DEPTH || out.len() >= MAX_FILES_PER_SOURCE || *entry_budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // M8: stop the moment either bound is hit — total entries EXAMINED, or
        // command files collected — so a hostile tree can't run the walk away.
        if *entry_budget == 0 || out.len() >= MAX_FILES_PER_SOURCE {
            return;
        }
        *entry_budget -= 1;
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_md_files(root, &path, depth + 1, out, entry_budget);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            && let Some(name) = namespaced_name(root, &path)
        {
            out.push((name, path));
        }
    }
}

/// Build the `git:commit` namespaced name from `root` and a `.md` file path.
fn namespaced_name(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let stem = path.file_stem()?.to_str()?;
    let mut segments: Vec<String> = relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str().map(str::to_owned),
            _ => None,
        })
        .collect();
    segments.push(stem.to_owned());
    let name = segments.join(":").to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}
