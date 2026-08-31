//! Daemon-owned, turn-scoped project-instruction loading.
//!
//! This is policy input, not a model-requested filesystem effect. Reads are
//! descriptor-relative, refuse symlinks, require regular UTF-8 files, and are
//! bounded before their contents can enter a provider request.

use haider_protocol::project_instructions::{
    ProjectInstructionFileFact, ProjectInstructionsLoaded,
};
#[cfg(unix)]
use rustix::fs::{FileType, Mode, OFlags};
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};

#[cfg(unix)]
type DirectoryHandle = OwnedFd;
#[cfg(windows)]
type DirectoryHandle = PathBuf;
#[cfg(unix)]
type DirectoryOpenError = rustix::io::Errno;
#[cfg(windows)]
type DirectoryOpenError = std::io::Error;

pub(crate) const MAX_PROJECT_INSTRUCTION_FILE_BYTES: usize = 48 * 1024;
pub(crate) const MAX_PROJECT_INSTRUCTION_TOTAL_BYTES: usize = 96 * 1024;
pub(crate) const MAX_PROJECT_INSTRUCTION_ANCESTORS: usize = 256;
const CANDIDATE_NAMES: [&str; 2] = ["HAIDER.md", "AGENTS.md"];

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LoadedProjectInstructions {
    files: Vec<LoadedProjectInstruction>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LoadedProjectInstruction {
    pub(crate) path: String,
    pub(crate) text: String,
    pub(crate) digest: String,
    pub(crate) truncated: bool,
}

impl LoadedProjectInstructions {
    pub(crate) fn prompt_entries(&self) -> Vec<(&str, &str)> {
        self.files
            .iter()
            .map(|file| (file.path.as_str(), file.text.as_str()))
            .collect()
    }

    pub(crate) fn fact(&self) -> ProjectInstructionsLoaded {
        ProjectInstructionsLoaded {
            files: self
                .files
                .iter()
                .map(|file| ProjectInstructionFileFact {
                    path: file.path.clone(),
                    digest: file.digest.clone(),
                    bytes: u64::try_from(file.text.len()).unwrap_or(u64::MAX),
                    truncated: file.truncated,
                })
                .collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn files(&self) -> &[LoadedProjectInstruction] {
        &self.files
    }
}

/// Loads one immutable logical-turn snapshot without entering the broker.
pub(crate) async fn load(canonical_cwd: &str) -> Option<LoadedProjectInstructions> {
    let cwd = PathBuf::from(canonical_cwd);
    match tokio::task::spawn_blocking(move || load_blocking(&cwd)).await {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::info!(
                target: "haider.worker",
                notice = true,
                ?error,
                "NOTICE: project-instruction loader stopped"
            );
            None
        }
    }
}

fn load_blocking(canonical_cwd: &Path) -> Option<LoadedProjectInstructions> {
    let Some(mut directory) = open_canonical_directory(canonical_cwd) else {
        instruction_notice(
            canonical_cwd,
            "workspace is not a canonical symlink-free directory",
        );
        return None;
    };
    let mut display_directory = canonical_cwd.to_path_buf();
    let mut nearest_first = Vec::new();
    let mut remaining = MAX_PROJECT_INSTRUCTION_TOTAL_BYTES;

    for depth in 0..MAX_PROJECT_INSTRUCTION_ANCESTORS {
        match load_directory_winner(&directory, &display_directory, remaining) {
            CandidateRead::Loaded(file) => {
                remaining = remaining.saturating_sub(file.text.len());
                nearest_first.push(file);
            }
            CandidateRead::BudgetExceeded => {
                push_budgeted_elision_marker(
                    &mut nearest_first,
                    &mut remaining,
                    "[project instruction aggregate elision]",
                    "project_instruction_aggregate_cap",
                    1,
                    None,
                );
                break;
            }
            CandidateRead::Missing | CandidateRead::Skipped => {}
        }

        let parent = match open_directory_at(&directory, Path::new("..")) {
            Ok(parent) => parent,
            Err(error) => {
                instruction_notice(
                    &display_directory,
                    &format!("parent directory could not be opened safely: {error}"),
                );
                break;
            }
        };
        if same_directory(&directory, &parent) {
            break;
        }
        if !display_directory.pop() {
            instruction_notice(&display_directory, "parent walk lost its absolute path");
            break;
        }
        let Some(expected_parent) = open_canonical_directory(&display_directory) else {
            instruction_notice(
                &display_directory,
                "parent changed or contains a symlink; upward walk stopped",
            );
            break;
        };
        if !same_directory(&parent, &expected_parent) {
            instruction_notice(
                &display_directory,
                "parent identity changed during upward walk",
            );
            break;
        }
        directory = parent;

        if depth + 1 == MAX_PROJECT_INSTRUCTION_ANCESTORS {
            push_budgeted_elision_marker(
                &mut nearest_first,
                &mut remaining,
                "[project instruction ancestor-depth elision]",
                "project_instruction_ancestor_depth_cap",
                0,
                Some((1, "ancestor_directory")),
            );
            instruction_notice(canonical_cwd, "bounded ancestor depth reached");
        }
    }

    // Budget is deliberately spent nearest-first so an oversized ancestor
    // cannot erase the more specific instructions. Composition reverses that
    // collection so nearest-to-cwd policy appears last and takes precedence.
    nearest_first.reverse();
    (!nearest_first.is_empty()).then_some(LoadedProjectInstructions {
        files: nearest_first,
    })
}

fn push_budgeted_elision_marker(
    nearest_first: &mut Vec<LoadedProjectInstruction>,
    remaining: &mut usize,
    path: &str,
    scope: &str,
    mut omitted_bytes_at_least: usize,
    omitted_items: Option<(u64, &str)>,
) {
    let marker = loop {
        let marker = omitted_items
            .map_or_else(
                || haider_tools::mark_text_elision("", 512, scope, omitted_bytes_at_least, false),
                |(count, unit)| {
                    haider_tools::mark_text_elision_with_items(
                        "",
                        512,
                        scope,
                        omitted_bytes_at_least,
                        false,
                        count,
                        unit,
                    )
                },
            )
            .text;
        if marker.len() <= *remaining {
            break marker;
        }
        let Some(removed) = nearest_first.pop() else {
            break marker;
        };
        *remaining = remaining.saturating_add(removed.text.len());
        omitted_bytes_at_least = omitted_bytes_at_least.saturating_add(removed.text.len());
    };
    *remaining = remaining.saturating_sub(marker.len());
    let digest = blake3::hash(marker.as_bytes()).to_hex().to_string();
    nearest_first.push(LoadedProjectInstruction {
        path: path.into(),
        text: marker,
        digest,
        truncated: true,
    });
}

fn load_directory_winner(
    directory: &DirectoryHandle,
    display_directory: &Path,
    remaining: usize,
) -> CandidateRead {
    for name in CANDIDATE_NAMES {
        let display_path = display_directory.join(name);
        match read_candidate(directory, name, &display_path, remaining) {
            CandidateRead::Loaded(file) => return CandidateRead::Loaded(file),
            CandidateRead::BudgetExceeded => return CandidateRead::BudgetExceeded,
            CandidateRead::Missing | CandidateRead::Skipped => {}
        }
    }
    CandidateRead::Missing
}

enum CandidateRead {
    Missing,
    Skipped,
    BudgetExceeded,
    Loaded(LoadedProjectInstruction),
}

fn read_candidate(
    directory: &DirectoryHandle,
    name: &str,
    display_path: &Path,
    remaining: usize,
) -> CandidateRead {
    read_candidate_platform(directory, name, display_path, remaining)
}

#[cfg(unix)]
fn read_candidate_platform(
    directory: &DirectoryHandle,
    name: &str,
    display_path: &Path,
    remaining: usize,
) -> CandidateRead {
    let file = match rustix::fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => file,
        Err(rustix::io::Errno::NOENT) => return CandidateRead::Missing,
        Err(error) => {
            instruction_notice(
                display_path,
                &format!("instruction file was skipped: {error}"),
            );
            return CandidateRead::Skipped;
        }
    };
    let metadata = match rustix::fs::fstat(&file) {
        Ok(metadata) => metadata,
        Err(error) => {
            instruction_notice(
                display_path,
                &format!("instruction file was skipped: {error}"),
            );
            return CandidateRead::Skipped;
        }
    };
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        instruction_notice(display_path, "instruction path is not a regular file");
        return CandidateRead::Skipped;
    }

    let cap = remaining.min(MAX_PROJECT_INSTRUCTION_FILE_BYTES);
    let read_limit = u64::try_from(cap.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(cap.saturating_add(1));
    if let Err(error) = fs::File::from(file)
        .take(read_limit)
        .read_to_end(&mut bytes)
    {
        instruction_notice(
            display_path,
            &format!("instruction file was unreadable: {error}"),
        );
        return CandidateRead::Skipped;
    }
    if cap == 0 && !bytes.is_empty() {
        return CandidateRead::BudgetExceeded;
    }

    let (text, truncated) = match bounded_utf8(bytes, cap) {
        BoundedUtf8::Loaded(text, truncated) => (text, truncated),
        BoundedUtf8::MarkerDoesNotFit => return CandidateRead::BudgetExceeded,
        BoundedUtf8::InvalidUtf8 => {
            instruction_notice(display_path, "instruction file was not valid bounded UTF-8");
            return CandidateRead::Skipped;
        }
    };
    let Some(path) = display_path.to_str() else {
        instruction_notice(display_path, "instruction path is not UTF-8");
        return CandidateRead::Skipped;
    };
    let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
    CandidateRead::Loaded(LoadedProjectInstruction {
        path: path.to_owned(),
        text,
        digest,
        truncated,
    })
}

#[cfg(windows)]
fn read_candidate_platform(
    directory: &DirectoryHandle,
    name: &str,
    display_path: &Path,
    remaining: usize,
) -> CandidateRead {
    let path = directory.join(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CandidateRead::Missing;
        }
        Err(error) => {
            instruction_notice(
                display_path,
                &format!("instruction file was skipped: {error}"),
            );
            return CandidateRead::Skipped;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        instruction_notice(display_path, "instruction path is not a regular file");
        return CandidateRead::Skipped;
    }
    let cap = remaining.min(MAX_PROJECT_INSTRUCTION_FILE_BYTES);
    let read_limit = u64::try_from(cap.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(cap.saturating_add(1));
    // Keep the Windows read binding shape aligned with the platform reader fixture.
    #[allow(unused_mut)]
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            instruction_notice(
                display_path,
                &format!("instruction file was unreadable: {error}"),
            );
            return CandidateRead::Skipped;
        }
    };
    if let Err(error) = file.take(read_limit).read_to_end(&mut bytes) {
        instruction_notice(
            display_path,
            &format!("instruction file was unreadable: {error}"),
        );
        return CandidateRead::Skipped;
    }
    loaded_candidate(display_path, bytes, cap)
}

#[cfg(windows)]
fn loaded_candidate(display_path: &Path, bytes: Vec<u8>, cap: usize) -> CandidateRead {
    if cap == 0 && !bytes.is_empty() {
        return CandidateRead::BudgetExceeded;
    }
    let (text, truncated) = match bounded_utf8(bytes, cap) {
        BoundedUtf8::Loaded(text, truncated) => (text, truncated),
        BoundedUtf8::MarkerDoesNotFit => return CandidateRead::BudgetExceeded,
        BoundedUtf8::InvalidUtf8 => {
            instruction_notice(display_path, "instruction file was not valid bounded UTF-8");
            return CandidateRead::Skipped;
        }
    };
    let Some(path) = display_path.to_str() else {
        instruction_notice(display_path, "instruction path is not UTF-8");
        return CandidateRead::Skipped;
    };
    let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
    CandidateRead::Loaded(LoadedProjectInstruction {
        path: path.to_owned(),
        text,
        digest,
        truncated,
    })
}

enum BoundedUtf8 {
    Loaded(String, bool),
    MarkerDoesNotFit,
    InvalidUtf8,
}

fn bounded_utf8(bytes: Vec<u8>, cap: usize) -> BoundedUtf8 {
    if bytes.len() <= cap {
        return String::from_utf8(bytes)
            .map(|text| BoundedUtf8::Loaded(text, false))
            .unwrap_or(BoundedUtf8::InvalidUtf8);
    }
    let candidate = &bytes[..cap];
    let prefix_end = match std::str::from_utf8(candidate) {
        Ok(_) => candidate.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return BoundedUtf8::InvalidUtf8,
    };
    let Ok(prefix) = std::str::from_utf8(&candidate[..prefix_end]) else {
        return BoundedUtf8::InvalidUtf8;
    };
    let omitted_bytes_at_least = bytes.len().saturating_sub(prefix.len());
    let elided = haider_tools::mark_text_elision(
        prefix,
        cap,
        "project_instruction_file_cap",
        omitted_bytes_at_least,
        false,
    );
    if elided.text.len() <= cap {
        BoundedUtf8::Loaded(elided.text, true)
    } else {
        BoundedUtf8::MarkerDoesNotFit
    }
}

#[cfg(unix)]
fn open_canonical_directory(path: &Path) -> Option<DirectoryHandle> {
    if !path.is_absolute() || fs::canonicalize(path).ok().as_deref() != Some(path) {
        return None;
    }
    let mut directory = rustix::fs::openat(
        rustix::fs::CWD,
        Path::new("/"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    for component in path.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        directory = open_directory_at(&directory, Path::new(component)).ok()?;
    }
    Some(directory)
}

#[cfg(unix)]
fn open_directory_at(
    directory: &DirectoryHandle,
    path: &Path,
) -> Result<DirectoryHandle, DirectoryOpenError> {
    rustix::fs::openat(
        directory,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

#[cfg(unix)]
fn same_directory(left: &DirectoryHandle, right: &DirectoryHandle) -> bool {
    let (Ok(left), Ok(right)) = (rustix::fs::fstat(left), rustix::fs::fstat(right)) else {
        return false;
    };
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(windows)]
fn open_canonical_directory(path: &Path) -> Option<DirectoryHandle> {
    if !path.is_absolute() || fs::canonicalize(path).ok().as_deref() != Some(path) {
        return None;
    }
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| !metadata.file_type().is_symlink() && metadata.is_dir())?;
    Some(path.to_path_buf())
}

#[cfg(windows)]
fn open_directory_at(
    directory: &DirectoryHandle,
    path: &Path,
) -> Result<DirectoryHandle, DirectoryOpenError> {
    let candidate = directory.join(path);
    let canonical = fs::canonicalize(&candidate)?;
    let metadata = fs::symlink_metadata(&candidate)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::other("path is not a real directory"));
    }
    Ok(canonical)
}

#[cfg(windows)]
fn same_directory(left: &DirectoryHandle, right: &DirectoryHandle) -> bool {
    left == right
}

fn instruction_notice(path: &Path, reason: &str) {
    tracing::info!(
        target: "haider.worker",
        notice = true,
        path = %path.display(),
        reason,
        "NOTICE: project instruction input skipped"
    );
}
