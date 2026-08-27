//! Repository-aware, workspace-confined file discovery for read-only tools.
//!
//! Discovery never shells out to `git`, follows a symlink, or asks the
//! `ignore` walker to discover parent controls. Repository-local ignore files
//! are snapshotted through anchored/no-follow handles and parsed with
//! BurntSushi's `ignore` crate before their rules are applied. Git's configured
//! global-exclude file is the sole intentional external policy input.

use crate::{ToolError, ToolResult};
use ignore::Match;
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

const IGNORE_CONTROL_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct WalkOptions {
    pub respect_gitignore: bool,
    pub include_hidden: bool,
    /// Hard cap on all enumerated entries, including hidden, ignored, and
    /// symlink entries. It is deliberately independent of retained results.
    pub max_files: usize,
    pub deadline: Option<Instant>,
}

#[derive(Debug)]
pub(crate) struct WalkOutcome {
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    pub truncated: bool,
    pub time_budget_reached: bool,
    /// Sensitive hidden files omitted by the walker. Callers apply their own
    /// path filter before adding these to a per-result skip count.
    pub hidden_sensitive_files: Vec<PathBuf>,
}

/// Finds the nearest repository marker between `start` and `workspace_root`,
/// inclusive. A linked-worktree `.git` file is a valid marker, but its external
/// target is never parsed. Inputs are canonical and the search is stopped at
/// the canonical workspace boundary.
pub(crate) fn detect_repo_root(workspace_root: &Path, start: &Path) -> Option<PathBuf> {
    if !start.starts_with(workspace_root) {
        return None;
    }
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink()
                && (metadata.file_type().is_dir() || metadata.file_type().is_file())
        }) {
            return Some(current);
        }
        if current == workspace_root {
            return None;
        }
        let parent = current.parent()?;
        if !parent.starts_with(workspace_root) {
            return None;
        }
        current = parent.to_path_buf();
    }
}

#[derive(Debug)]
struct LocalIgnore {
    base: PathBuf,
    matcher: Gitignore,
}

/// Returns path-sorted regular files relative to `workspace_root`.
pub(crate) fn walk_files(
    workspace_root: &Path,
    search_root: &Path,
    options: WalkOptions,
) -> ToolResult<WalkOutcome> {
    if options.max_files == 0 {
        return Err(ToolError::invalid_argument(
            "repository walk max_files must be greater than zero",
        ));
    }
    let canonical_workspace = std::fs::canonicalize(workspace_root).map_err(|error| {
        ToolError::io("canonicalize repository workspace", workspace_root, error)
    })?;
    let canonical_search = std::fs::canonicalize(search_root).map_err(|error| {
        ToolError::io("canonicalize repository search root", search_root, error)
    })?;
    if !canonical_search.starts_with(&canonical_workspace) {
        return Err(ToolError::WorkspaceBoundary {
            workspace_root: canonical_workspace.clone(),
            requested_path: search_root.to_path_buf(),
            resolved_path: Some(canonical_search.clone()),
        });
    }
    if is_git_metadata_name(&canonical_search) {
        return Ok(empty_walk());
    }
    let workspace_root = canonical_workspace.as_path();
    let search_root = canonical_search.as_path();
    let repo_root = detect_repo_root(workspace_root, search_root);
    let walk_root = repo_root.as_deref().unwrap_or(search_root);

    // This is an enumeration-only walker. Every built-in ignore source is
    // disabled so it cannot path-open a control after validation or discover
    // parent metadata. Rules are applied from anchored snapshots below.
    let requested = search_root.to_path_buf();
    let filter_root = walk_root.to_path_buf();
    let mut builder = WalkBuilder::new(walk_root);
    builder
        .follow_links(false)
        .standard_filters(false)
        .parents(false)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .sort_by_file_path(|left, right| {
            is_gitignore_name(right)
                .cmp(&is_gitignore_name(left))
                .then_with(|| left.cmp(right))
        })
        .filter_entry(move |entry| {
            let path = entry.path();
            if path != filter_root && is_git_metadata_name(path) {
                return false;
            }
            let ancestor_gitignore = is_gitignore_name(path)
                && path
                    .parent()
                    .is_some_and(|parent| requested.starts_with(parent));
            path == filter_root
                || requested.starts_with(path)
                || path.starts_with(&requested)
                || ancestor_gitignore
        });

    let mut raw_files = Vec::new();
    let mut raw_directories = Vec::new();
    let mut entries_seen = 0usize;
    let mut truncated = false;
    let mut time_budget_reached = false;
    for entry in builder.build() {
        if options
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            time_budget_reached = true;
            break;
        }
        let entry = entry
            .map_err(|error| ToolError::io("walk repository", search_root, error.to_string()))?;
        if entry.path() == walk_root {
            if walk_root.starts_with(search_root) {
                raw_directories.push(walk_root.to_path_buf());
            }
            continue;
        }
        entries_seen = entries_seen.saturating_add(1);
        if entries_seen > options.max_files {
            truncated = true;
            break;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !entry.path().starts_with(search_root) {
            // Ancestor `.gitignore` files between repo root and search root are
            // retained separately even though ordinary ancestor files are not.
            if file_type.is_file()
                && is_gitignore_name(entry.path())
                && search_root.starts_with(entry.path().parent().unwrap_or(walk_root))
            {
                raw_files.push(entry.path().to_path_buf());
            }
            continue;
        }
        if file_type.is_dir() {
            raw_directories.push(entry.path().to_path_buf());
        } else if file_type.is_file() {
            raw_files.push(entry.path().to_path_buf());
        }
    }

    let loaded = if options.respect_gitignore {
        load_ignore_rules(workspace_root, repo_root.as_deref(), &raw_files)?
    } else {
        LoadedIgnoreRules {
            global: Gitignore::empty(),
            repository_exclude: Gitignore::empty(),
            local: Vec::new(),
        }
    };
    let ignore_rules = IgnoreRules {
        global: &loaded.global,
        repository_exclude: &loaded.repository_exclude,
        local: &loaded.local,
        boundary: repo_root.as_deref().unwrap_or(search_root),
    };

    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut hidden_sensitive_files = Vec::new();
    for absolute in raw_directories {
        if options.respect_gitignore && ignore_rules.path_or_parent_ignored(&absolute, true) {
            continue;
        }
        let relative = workspace_relative(workspace_root, &absolute)?;
        if options.include_hidden || !is_hidden_path(relative, &absolute) {
            directories.push(relative.to_path_buf());
        }
    }
    for absolute in raw_files {
        if !absolute.starts_with(search_root) {
            continue;
        }
        if options.respect_gitignore && ignore_rules.path_or_parent_ignored(&absolute, false) {
            continue;
        }
        let relative = workspace_relative(workspace_root, &absolute)?;
        if !options.include_hidden && is_hidden_path(relative, &absolute) {
            if crate::redact::is_sensitive_path(relative) {
                hidden_sensitive_files.push(relative.to_path_buf());
            }
            continue;
        }
        files.push(relative.to_path_buf());
    }
    files.sort();
    files.dedup();
    directories.sort();
    directories.dedup();
    hidden_sensitive_files.sort();
    hidden_sensitive_files.dedup();
    Ok(WalkOutcome {
        files,
        directories,
        truncated,
        time_budget_reached,
        hidden_sensitive_files,
    })
}

fn empty_walk() -> WalkOutcome {
    WalkOutcome {
        files: Vec::new(),
        directories: Vec::new(),
        truncated: false,
        time_budget_reached: false,
        hidden_sensitive_files: Vec::new(),
    }
}

struct IgnoreRules<'a> {
    global: &'a Gitignore,
    repository_exclude: &'a Gitignore,
    local: &'a [LocalIgnore],
    boundary: &'a Path,
}

impl IgnoreRules<'_> {
    fn path_or_parent_ignored(&self, path: &Path, is_dir: bool) -> bool {
        for ancestor in path
            .ancestors()
            .skip(1)
            .take_while(|ancestor| ancestor.starts_with(self.boundary))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            if self.decision(ancestor, true) {
                return true;
            }
        }
        self.decision(path, is_dir)
    }

    fn decision(&self, path: &Path, is_dir: bool) -> bool {
        let mut ignored = match_is_ignore(self.global.matched(path, is_dir), false);
        ignored = match_is_ignore(self.repository_exclude.matched(path, is_dir), ignored);
        for local in self.local {
            if path != local.base && path.starts_with(&local.base) {
                ignored = match_is_ignore(local.matcher.matched(path, is_dir), ignored);
            }
        }
        ignored
    }
}

fn match_is_ignore<T>(matched: Match<T>, prior: bool) -> bool {
    match matched {
        Match::Ignore(_) => true,
        Match::Whitelist(_) => false,
        Match::None => prior,
    }
}

struct LoadedIgnoreRules {
    global: Gitignore,
    repository_exclude: Gitignore,
    local: Vec<LocalIgnore>,
}

fn load_ignore_rules(
    workspace_root: &Path,
    repo_root: Option<&Path>,
    files: &[PathBuf],
) -> ToolResult<LoadedIgnoreRules> {
    // This intentionally honors Git's user-configured global policy. Unlike
    // WalkBuilder's git flags, it does not discover repository parents.
    let (global, _) = GitignoreBuilder::new(workspace_root).build_global();
    let repository_exclude = if let Some(repo_root) = repo_root {
        let metadata = repo_root.join(".git");
        if std::fs::symlink_metadata(&metadata).is_ok_and(|m| m.file_type().is_dir()) {
            let relative = workspace_relative(workspace_root, &metadata.join("info/exclude"))?;
            build_anchored_ignore(workspace_root, repo_root, relative)?
        } else {
            Gitignore::empty()
        }
    } else {
        Gitignore::empty()
    };
    let mut controls = files
        .iter()
        .filter(|path| is_gitignore_name(path))
        .cloned()
        .collect::<Vec<_>>();
    controls.sort_by_key(|path| (path.components().count(), path.clone()));
    controls.dedup();
    let mut local = Vec::with_capacity(controls.len());
    for control in controls {
        let Some(base) = control.parent() else {
            continue;
        };
        let relative = workspace_relative(workspace_root, &control)?;
        local.push(LocalIgnore {
            base: base.to_path_buf(),
            matcher: build_anchored_ignore(workspace_root, base, relative)?,
        });
    }
    Ok(LoadedIgnoreRules {
        global,
        repository_exclude,
        local,
    })
}

fn build_anchored_ignore(
    workspace_root: &Path,
    base: &Path,
    relative: &Path,
) -> ToolResult<Gitignore> {
    let Some(contents) = read_anchored_optional(workspace_root, relative)? else {
        return Ok(Gitignore::empty());
    };
    let mut builder = GitignoreBuilder::new(base);
    for (index, line) in contents.lines().enumerate() {
        let line = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        builder
            .add_line(Some(workspace_root.join(relative)), line)
            .map_err(|error| ToolError::InvalidArgument {
                message: format!(
                    "invalid ignore control {} at line {}: {error}",
                    relative.display(),
                    index.saturating_add(1)
                ),
            })?;
    }
    builder.build().map_err(|error| ToolError::InvalidArgument {
        message: format!("invalid ignore control {}: {error}", relative.display()),
    })
}

#[cfg(unix)]
fn read_anchored_optional(workspace_root: &Path, relative: &Path) -> ToolResult<Option<String>> {
    use rustix::fs::{Mode, OFlags};

    let mut directory = haider_platform::open_workspace_directory(workspace_root)
        .map_err(|error| ToolError::io("open ignore workspace", workspace_root, error))?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Err(ToolError::WorkspaceBoundary {
                workspace_root: workspace_root.to_path_buf(),
                requested_path: relative.to_path_buf(),
                resolved_path: None,
            });
        };
        let is_leaf = components.peek().is_none();
        let flags = OFlags::RDONLY
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC
            | if is_leaf {
                OFlags::empty()
            } else {
                OFlags::DIRECTORY
            };
        match rustix::fs::openat(&directory, component, flags, Mode::empty()) {
            Ok(opened) => directory = opened,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => {
                return Err(ToolError::PathChanged {
                    path: workspace_root.join(relative),
                    message: error.to_string(),
                });
            }
        }
    }
    read_ignore_file(
        std::fs::File::from(directory),
        workspace_root.join(relative),
    )
    .map(Some)
}

#[cfg(windows)]
fn read_anchored_optional(workspace_root: &Path, relative: &Path) -> ToolResult<Option<String>> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    let Some(leaf) = relative.file_name() else {
        return Err(ToolError::invalid_argument(
            "ignore control has no file name",
        ));
    };
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let root = haider_platform::open_workspace_directory(workspace_root)
        .map_err(|error| ToolError::io("open ignore workspace", workspace_root, error))?;
    let parent = match haider_platform::open_workspace_subdirectory(root, parent_relative, false) {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ToolError::PathChanged {
                path: workspace_root.join(parent_relative),
                message: error.to_string(),
            });
        }
    };
    let path = parent.path().join(leaf);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ToolError::io("open ignore control", &path, error)),
    };
    if file
        .metadata()
        .map_err(|error| ToolError::io("inspect ignore control", &path, error))?
        .file_attributes()
        & FILE_ATTRIBUTE_REPARSE_POINT
        != 0
    {
        return Err(ToolError::PathChanged {
            path,
            message: "ignore control is a reparse point".into(),
        });
    }
    read_ignore_file(file, path).map(Some)
}

fn read_ignore_file(file: std::fs::File, path: PathBuf) -> ToolResult<String> {
    let mut bytes = Vec::new();
    let byte_limit = u64::try_from(IGNORE_CONTROL_MAX_BYTES.saturating_add(1)).unwrap_or(u64::MAX);
    file.take(byte_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| ToolError::io("read ignore control", &path, error))?;
    if bytes.len() > IGNORE_CONTROL_MAX_BYTES {
        return Err(ToolError::invalid_argument(format!(
            "ignore control {} exceeds {IGNORE_CONTROL_MAX_BYTES} bytes",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|error| ToolError::InvalidArgument {
        message: format!("ignore control {} is not UTF-8: {error}", path.display()),
    })
}

fn workspace_relative<'a>(workspace_root: &Path, path: &'a Path) -> ToolResult<&'a Path> {
    path.strip_prefix(workspace_root)
        .map_err(|_| ToolError::WorkspaceBoundary {
            workspace_root: workspace_root.to_path_buf(),
            requested_path: path.to_path_buf(),
            resolved_path: Some(path.to_path_buf()),
        })
}

fn is_git_metadata_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
}

fn is_gitignore_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(".gitignore"))
}

fn is_hidden_path(relative: &Path, _path: &Path) -> bool {
    if relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.starts_with('.') && name != "." && name != "..")
    }) {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        _path
            .ancestors()
            .take(relative.components().count())
            .any(|path| {
                std::fs::symlink_metadata(path).is_ok_and(|metadata| {
                    metadata.file_attributes()
                        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN
                        != 0
                })
            })
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
#[path = "repo/tests/repo_tests.rs"]
mod tests;
