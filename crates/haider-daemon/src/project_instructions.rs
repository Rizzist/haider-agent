//! Daemon-owned, turn-scoped project-instruction loading.
//!
//! This is policy input, not a model-requested filesystem effect. Reads are
//! descriptor-relative, refuse symlinks, require regular UTF-8 files, and are
//! bounded before their contents can enter a provider request.

use haider_protocol::project_instructions::{
    ProjectInstructionFileFact, ProjectInstructionsLoaded,
};
#[cfg(unix)]
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
#[cfg(unix)]
use std::collections::VecDeque;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

#[cfg(unix)]
type DirectoryHandle = OwnedFd;
#[cfg(windows)]
type DirectoryHandle = PathBuf;
#[cfg(unix)]
type DirectoryIdentity = (u64, u64);
#[cfg(windows)]
type DirectoryIdentity = PathBuf;
#[cfg(unix)]
type DirectoryOpenError = rustix::io::Errno;
#[cfg(windows)]
type DirectoryOpenError = std::io::Error;

pub(crate) const MAX_PROJECT_INSTRUCTION_FILE_BYTES: usize = 48 * 1024;
pub(crate) const MAX_PROJECT_INSTRUCTION_TOTAL_BYTES: usize = 96 * 1024;
pub(crate) const MAX_PROJECT_INSTRUCTION_ANCESTORS: usize = 256;
const CANDIDATE_NAMES: [&str; 2] = ["HAIDER.md", "AGENTS.md"];
#[cfg(unix)]
const PROJECT_INSTRUCTION_CACHE_ENTRIES: usize = 4;
#[cfg(unix)]
const PROJECT_INSTRUCTION_CACHE_BYTES: usize = 256 * 1024;

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
    #[cfg(unix)]
    {
        let before = discovery_stamp(canonical_cwd);
        if let Some(stamp) = before.as_ref()
            && let Some(loaded) = instruction_cache()
                .lock_or_recover()
                .lookup(canonical_cwd, stamp)
        {
            #[cfg(test)]
            PROJECT_INSTRUCTION_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return loaded;
        }

        let loaded = load_uncached_blocking(canonical_cwd);
        let after = discovery_stamp(canonical_cwd);
        if let (Some(before), Some(after)) = (before, after)
            && before == after
        {
            instruction_cache().lock_or_recover().insert(
                canonical_cwd.to_path_buf(),
                after,
                loaded.clone(),
            );
        }
        loaded
    }

    #[cfg(not(unix))]
    load_uncached_blocking(canonical_cwd)
}

fn load_uncached_blocking(canonical_cwd: &Path) -> Option<LoadedProjectInstructions> {
    let Some((mut directory, identities)) = open_canonical_directory_chain(canonical_cwd) else {
        instruction_notice(
            canonical_cwd,
            "workspace is not a canonical symlink-free directory",
        );
        return None;
    };
    let mut identity_index = identities.len().checked_sub(1)?;
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

        if identity_index == 0 {
            break;
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
        if !display_directory.pop() {
            instruction_notice(&display_directory, "parent walk lost its absolute path");
            break;
        }
        identity_index = identity_index.saturating_sub(1);
        if directory_identity(&parent).as_ref() != Some(&identities[identity_index]) {
            instruction_notice(
                &display_directory,
                "parent identity changed during upward walk",
            );
            break;
        }
        directory = parent;

        if depth + 1 == MAX_PROJECT_INSTRUCTION_ANCESTORS && identity_index > 0 {
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

#[cfg(unix)]
#[derive(Clone, PartialEq, Eq)]
struct DiscoveryStamp {
    directories: Vec<DirectoryIdentity>,
    candidates: Vec<[CandidateStamp; 2]>,
    ancestor_depth_capped: bool,
}

#[cfg(unix)]
#[derive(Clone, PartialEq, Eq)]
enum CandidateStamp {
    Missing,
    Present {
        device: u64,
        inode: u64,
        mode: u64,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
        changed_seconds: i64,
        changed_nanoseconds: i64,
    },
}

#[cfg(unix)]
fn discovery_stamp(canonical_cwd: &Path) -> Option<DiscoveryStamp> {
    let (mut directory, identities) = open_canonical_directory_chain(canonical_cwd)?;
    let mut identity_index = identities.len().checked_sub(1)?;
    let mut directories = Vec::new();
    let mut candidates = Vec::new();

    for _ in 0..MAX_PROJECT_INSTRUCTION_ANCESTORS {
        directories.push(identities[identity_index]);
        candidates.push([
            candidate_stamp(&directory, CANDIDATE_NAMES[0])?,
            candidate_stamp(&directory, CANDIDATE_NAMES[1])?,
        ]);
        if identity_index == 0 {
            break;
        }
        let parent = open_directory_at(&directory, Path::new("..")).ok()?;
        identity_index = identity_index.saturating_sub(1);
        if directory_identity(&parent).as_ref() != Some(&identities[identity_index]) {
            return None;
        }
        directory = parent;
    }

    Some(DiscoveryStamp {
        directories,
        candidates,
        ancestor_depth_capped: identity_index > 0,
    })
}

#[cfg(unix)]
fn candidate_stamp(directory: &DirectoryHandle, name: &str) -> Option<CandidateStamp> {
    let metadata = match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(rustix::io::Errno::NOENT) => return Some(CandidateStamp::Missing),
        Err(_) => return None,
    };
    Some(CandidateStamp::Present {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
        mode: metadata.st_mode as u64,
        size: metadata.st_size.max(0) as u64,
        modified_seconds: metadata.st_mtime,
        modified_nanoseconds: metadata.st_mtime_nsec as i64,
        changed_seconds: metadata.st_ctime,
        changed_nanoseconds: metadata.st_ctime_nsec as i64,
    })
}

#[cfg(unix)]
#[derive(Clone)]
struct InstructionCacheEntry {
    cwd: PathBuf,
    stamp: DiscoveryStamp,
    loaded: Option<LoadedProjectInstructions>,
    retained_bytes: usize,
}

#[cfg(unix)]
#[derive(Default)]
struct InstructionCache {
    entries: VecDeque<InstructionCacheEntry>,
    retained_bytes: usize,
}

#[cfg(unix)]
impl InstructionCache {
    fn lookup(
        &mut self,
        cwd: &Path,
        stamp: &DiscoveryStamp,
    ) -> Option<Option<LoadedProjectInstructions>> {
        let position = self.entries.iter().position(|entry| entry.cwd == cwd)?;
        if self.entries[position].stamp != *stamp {
            let removed = self.entries.remove(position)?;
            self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes);
            return None;
        }
        let entry = self.entries.remove(position)?;
        let loaded = entry.loaded.clone();
        self.entries.push_back(entry);
        Some(loaded)
    }

    fn insert(
        &mut self,
        cwd: PathBuf,
        stamp: DiscoveryStamp,
        loaded: Option<LoadedProjectInstructions>,
    ) {
        if let Some(position) = self.entries.iter().position(|entry| entry.cwd == cwd)
            && let Some(removed) = self.entries.remove(position)
        {
            self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes);
        }
        let retained_bytes = cache_entry_bytes(&cwd, &stamp, loaded.as_ref());
        if retained_bytes > PROJECT_INSTRUCTION_CACHE_BYTES {
            return;
        }
        while self.entries.len() >= PROJECT_INSTRUCTION_CACHE_ENTRIES
            || self.retained_bytes.saturating_add(retained_bytes) > PROJECT_INSTRUCTION_CACHE_BYTES
        {
            let Some(removed) = self.entries.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes);
        }
        self.retained_bytes = self.retained_bytes.saturating_add(retained_bytes);
        self.entries.push_back(InstructionCacheEntry {
            cwd,
            stamp,
            loaded,
            retained_bytes,
        });
    }
}

#[cfg(unix)]
fn cache_entry_bytes(
    cwd: &Path,
    stamp: &DiscoveryStamp,
    loaded: Option<&LoadedProjectInstructions>,
) -> usize {
    let snapshot_bytes = loaded.map_or(0, |loaded| {
        loaded
            .files
            .iter()
            .map(|file| file.path.len() + file.text.len() + file.digest.len())
            .sum()
    });
    cwd.as_os_str().len()
        + snapshot_bytes
        + stamp.directories.len() * std::mem::size_of::<DirectoryIdentity>()
        + stamp.candidates.len() * std::mem::size_of::<[CandidateStamp; 2]>()
}

#[cfg(unix)]
trait LockOrRecover<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

#[cfg(unix)]
impl<T> LockOrRecover<T> for Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(unix)]
fn instruction_cache() -> &'static Mutex<InstructionCache> {
    static CACHE: OnceLock<Mutex<InstructionCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(InstructionCache::default()))
}

#[cfg(all(test, unix))]
static PROJECT_INSTRUCTION_CACHE_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(test, unix))]
pub(crate) fn project_instruction_cache_hits() -> usize {
    PROJECT_INSTRUCTION_CACHE_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(all(test, unix))]
pub(crate) fn project_instruction_cache_usage() -> (usize, usize) {
    let cache = instruction_cache().lock_or_recover();
    (cache.entries.len(), cache.retained_bytes)
}

#[cfg(all(test, unix))]
pub(crate) fn canonical_directory_chain_len(path: &Path) -> Option<usize> {
    open_canonical_directory_chain(path).map(|(_, identities)| identities.len())
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
fn open_canonical_directory_chain(
    path: &Path,
) -> Option<(DirectoryHandle, Vec<DirectoryIdentity>)> {
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
    let mut identities = vec![directory_identity(&directory)?];
    for component in path.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        directory = open_directory_at(&directory, Path::new(component)).ok()?;
        identities.push(directory_identity(&directory)?);
    }
    Some((directory, identities))
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
fn directory_identity(directory: &DirectoryHandle) -> Option<DirectoryIdentity> {
    let metadata = rustix::fs::fstat(directory).ok()?;
    Some((metadata.st_dev as u64, metadata.st_ino as u64))
}

#[cfg(windows)]
fn open_canonical_directory_chain(
    path: &Path,
) -> Option<(DirectoryHandle, Vec<DirectoryIdentity>)> {
    if !path.is_absolute() || fs::canonicalize(path).ok().as_deref() != Some(path) {
        return None;
    }
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| !metadata.file_type().is_symlink() && metadata.is_dir())?;
    let mut identities = path.ancestors().map(Path::to_path_buf).collect::<Vec<_>>();
    identities.reverse();
    Some((path.to_path_buf(), identities))
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
fn directory_identity(directory: &DirectoryHandle) -> Option<DirectoryIdentity> {
    Some(directory.clone())
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
