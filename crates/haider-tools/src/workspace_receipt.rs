use crate::repo::{WalkEntry, WalkOptions, detect_repo_root, walk_files};
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;

const WORKSPACE_RECEIPT_DOMAIN: &[u8] = b"haider.workspace-state.v3";
const WORKSPACE_RECEIPT_MAX_ENTRIES: usize = 4_096;
const WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES: u64 = 16 * 1024 * 1024;
const WORKSPACE_RECEIPT_GIT_OUTPUT_BYTES: usize = 1024 * 1024;
const WORKSPACE_RECEIPT_WALL_TIME: Duration = Duration::from_millis(500);
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(1);
const GIT_INDEX_HEADER_BYTES: u64 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceReceiptStrategy {
    GitStatus,
    RepositoryWalk,
    NotEnumerated,
}

impl WorkspaceReceiptStrategy {
    fn as_str(self) -> &'static str {
        match self {
            Self::GitStatus => "git_status",
            Self::RepositoryWalk => "repository_walk",
            Self::NotEnumerated => "not_enumerated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceReceiptUnknownReason {
    NonRepository,
    EntryLimit,
    ContentLimit,
    WallTimeLimit,
    GitUnavailable,
    GitFailed,
    GitOutputLimit,
    GitStatusMalformed,
    RepositoryWalkFailed,
    PathEscaped,
    SymlinkOrReparsePoint,
    DirectoryOrSpecialEntry,
    EntryChangedDuringRead,
    EntryReadFailed,
    ReceiptWorkerFailed,
    ConcurrentOrInterleavedMutation,
}

impl WorkspaceReceiptUnknownReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::NonRepository => "not_enumerated_non_repository",
            Self::EntryLimit => "entry_limit",
            Self::ContentLimit => "content_limit",
            Self::WallTimeLimit => "wall_time_limit",
            Self::GitUnavailable => "git_unavailable",
            Self::GitFailed => "git_failed",
            Self::GitOutputLimit => "git_output_limit",
            Self::GitStatusMalformed => "git_status_malformed",
            Self::RepositoryWalkFailed => "repository_walk_failed",
            Self::PathEscaped => "path_escaped",
            Self::SymlinkOrReparsePoint => "symlink_or_reparse_point",
            Self::DirectoryOrSpecialEntry => "directory_or_special_entry",
            Self::EntryChangedDuringRead => "entry_changed_during_read",
            Self::EntryReadFailed => "entry_read_failed",
            Self::ReceiptWorkerFailed => "receipt_worker_failed",
            Self::ConcurrentOrInterleavedMutation => "concurrent_or_interleaved_mutation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceReceiptCoverage {
    Complete,
    Unknown(WorkspaceReceiptUnknownReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStateReceipt {
    fingerprint: String,
    pub coverage: WorkspaceReceiptCoverage,
    pub strategy: WorkspaceReceiptStrategy,
    /// Whether the numeric visit/read counters are exact. A hard outer wall
    /// timeout cannot recover counters from its still-bounded worker.
    pub counts_known: bool,
    pub entries_visited: usize,
    pub content_bytes_read: u64,
}

impl WorkspaceStateReceipt {
    #[must_use]
    pub fn mutation_digest(&self) -> String {
        match self.coverage {
            WorkspaceReceiptCoverage::Complete => self.fingerprint.clone(),
            WorkspaceReceiptCoverage::Unknown(reason) if self.counts_known => format!(
                "{};coverage=unknown;reason={};strategy={};entries={};bytes={}",
                self.fingerprint,
                reason.as_str(),
                self.strategy.as_str(),
                self.entries_visited,
                self.content_bytes_read,
            ),
            WorkspaceReceiptCoverage::Unknown(reason) => format!(
                "{};coverage=unknown;reason={};strategy={};entries=unreported;bytes=unreported",
                self.fingerprint,
                reason.as_str(),
                self.strategy.as_str(),
            ),
        }
    }

    fn unknown(
        strategy: WorkspaceReceiptStrategy,
        reason: WorkspaceReceiptUnknownReason,
        entries_visited: usize,
        content_bytes_read: u64,
    ) -> Self {
        let mut hasher = receipt_hasher(strategy);
        update_field(&mut hasher, b"unknown");
        update_field(&mut hasher, reason.as_str().as_bytes());
        hasher.update(&(entries_visited as u64).to_be_bytes());
        hasher.update(&content_bytes_read.to_be_bytes());
        Self {
            fingerprint: finalize(hasher),
            coverage: WorkspaceReceiptCoverage::Unknown(reason),
            strategy,
            counts_known: true,
            entries_visited,
            content_bytes_read,
        }
    }

    fn unknown_unreported(
        strategy: WorkspaceReceiptStrategy,
        reason: WorkspaceReceiptUnknownReason,
    ) -> Self {
        let mut receipt = Self::unknown(strategy, reason, 0, 0);
        receipt.counts_known = false;
        receipt
    }

    pub(crate) fn worker_failed() -> Self {
        Self::unknown_unreported(
            WorkspaceReceiptStrategy::NotEnumerated,
            WorkspaceReceiptUnknownReason::ReceiptWorkerFailed,
        )
    }
}

struct ReceiptBuilder {
    hasher: blake3::Hasher,
    strategy: WorkspaceReceiptStrategy,
    entries_visited: usize,
    content_bytes_read: u64,
    content_budget: u64,
    counts_known: bool,
    unknown: Option<WorkspaceReceiptUnknownReason>,
    deadline: Instant,
}

impl ReceiptBuilder {
    fn new(strategy: WorkspaceReceiptStrategy, deadline: Instant) -> Self {
        Self {
            hasher: receipt_hasher(strategy),
            strategy,
            entries_visited: 0,
            content_bytes_read: 0,
            content_budget: WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES,
            counts_known: true,
            unknown: None,
            deadline,
        }
    }

    fn mark_unknown(&mut self, reason: WorkspaceReceiptUnknownReason) {
        self.unknown.get_or_insert(reason);
    }

    fn hash_path(
        &mut self,
        reader: &AnchoredWorkspaceReader,
        relative: &Path,
    ) -> Result<(), WorkspaceReceiptUnknownReason> {
        if Instant::now() >= self.deadline {
            return Err(WorkspaceReceiptUnknownReason::WallTimeLimit);
        }
        self.entries_visited = self.entries_visited.saturating_add(1);
        if self.entries_visited > WORKSPACE_RECEIPT_MAX_ENTRIES {
            return Err(WorkspaceReceiptUnknownReason::EntryLimit);
        }
        update_path_field(&mut self.hasher, relative);
        let Some(mut file) = reader.open_regular_file(relative)? else {
            self.hasher.update(b"missing");
            return Ok(());
        };
        let before = file
            .metadata()
            .map_err(|_| WorkspaceReceiptUnknownReason::EntryReadFailed)?;
        hash_metadata(&mut self.hasher, &before);
        let observed_len = before.len();
        let permitted = observed_len.min(self.content_budget);
        let mut read_total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        while read_total < permitted {
            if Instant::now() >= self.deadline {
                return Err(WorkspaceReceiptUnknownReason::WallTimeLimit);
            }
            let remaining = usize::try_from(permitted - read_total).unwrap_or(usize::MAX);
            let chunk_len = buffer.len().min(remaining);
            let read = file
                .read(&mut buffer[..chunk_len])
                .map_err(|_| WorkspaceReceiptUnknownReason::EntryReadFailed)?;
            if read == 0 {
                return Err(WorkspaceReceiptUnknownReason::EntryChangedDuringRead);
            }
            self.hasher.update(&buffer[..read]);
            read_total = read_total.saturating_add(read as u64);
        }
        self.content_budget = self.content_budget.saturating_sub(read_total);
        self.content_bytes_read = self.content_bytes_read.saturating_add(read_total);
        if read_total != observed_len {
            self.hasher.update(b"content-elided");
            self.hasher
                .update(&observed_len.saturating_sub(read_total).to_be_bytes());
            return Err(WorkspaceReceiptUnknownReason::ContentLimit);
        }
        self.hasher.update(b"content-complete");
        let after = file
            .metadata()
            .map_err(|_| WorkspaceReceiptUnknownReason::EntryReadFailed)?;
        if !metadata_matches(&before, &after) {
            return Err(WorkspaceReceiptUnknownReason::EntryChangedDuringRead);
        }
        Ok(())
    }

    fn finish(mut self) -> WorkspaceStateReceipt {
        if let Some(reason) = self.unknown {
            update_field(&mut self.hasher, b"unknown");
            update_field(&mut self.hasher, reason.as_str().as_bytes());
        } else {
            update_field(&mut self.hasher, b"complete");
        }
        WorkspaceStateReceipt {
            fingerprint: finalize(self.hasher),
            coverage: self.unknown.map_or(
                WorkspaceReceiptCoverage::Complete,
                WorkspaceReceiptCoverage::Unknown,
            ),
            strategy: self.strategy,
            counts_known: self.counts_known,
            entries_visited: self.entries_visited,
            content_bytes_read: self.content_bytes_read,
        }
    }
}

fn receipt_hasher(strategy: WorkspaceReceiptStrategy) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(WORKSPACE_RECEIPT_DOMAIN);
    update_field(&mut hasher, strategy.as_str().as_bytes());
    hasher
}

fn update_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(unix)]
fn update_path_field(hasher: &mut blake3::Hasher, path: &Path) {
    update_field(hasher, path.as_os_str().as_bytes());
}

#[cfg(windows)]
fn update_path_field(hasher: &mut blake3::Hasher, path: &Path) {
    update_field(hasher, path.to_string_lossy().as_bytes());
}

fn finalize(hasher: blake3::Hasher) -> String {
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Returns within the receipt wall budget even when an individual filesystem
/// operation stalls. The worker still has independent entry/content/deadline
/// caps, so a timed-out worker cannot continue into an unbounded traversal.
#[must_use]
pub fn workspace_state_receipt(root: &Path) -> WorkspaceStateReceipt {
    let root = root.to_path_buf();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("haider-workspace-receipt".into())
        .spawn(move || {
            let _ = sender.send(compute_workspace_state_receipt(&root));
        });
    if worker.is_err() {
        return WorkspaceStateReceipt::unknown_unreported(
            WorkspaceReceiptStrategy::NotEnumerated,
            WorkspaceReceiptUnknownReason::ReceiptWorkerFailed,
        );
    }
    match receiver.recv_timeout(WORKSPACE_RECEIPT_WALL_TIME) {
        Ok(receipt) => receipt,
        Err(mpsc::RecvTimeoutError::Timeout) => WorkspaceStateReceipt::unknown_unreported(
            WorkspaceReceiptStrategy::NotEnumerated,
            WorkspaceReceiptUnknownReason::WallTimeLimit,
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => WorkspaceStateReceipt::unknown_unreported(
            WorkspaceReceiptStrategy::NotEnumerated,
            WorkspaceReceiptUnknownReason::ReceiptWorkerFailed,
        ),
    }
}

#[must_use]
pub fn workspace_state_digest(root: &Path) -> String {
    workspace_state_receipt(root).mutation_digest()
}

fn compute_workspace_state_receipt(root: &Path) -> WorkspaceStateReceipt {
    compute_workspace_state_receipt_with_git(root, OsStr::new("git"))
}

fn compute_workspace_state_receipt_with_git(
    root: &Path,
    git_program: &OsStr,
) -> WorkspaceStateReceipt {
    let deadline = Instant::now() + WORKSPACE_RECEIPT_WALL_TIME;
    if detect_repo_root(root, root).as_deref() != Some(root) {
        return WorkspaceStateReceipt::unknown(
            WorkspaceReceiptStrategy::NotEnumerated,
            WorkspaceReceiptUnknownReason::NonRepository,
            0,
            0,
        );
    }
    // Enumerate with our own unsorted, capped walker before Git. This is the
    // constructional bound: an opaque Git process is never launched for a tree
    // that we could not enumerate within the entry/content/wall limits.
    let walked = repository_walk_receipt(root, deadline);
    if walked.coverage != WorkspaceReceiptCoverage::Complete {
        return walked;
    }
    let Some(index) = repository_index_entry_count(root) else {
        // Linked worktrees/submodules use a `.git` file whose target may live
        // outside the workspace. Keep the complete anchored walk rather than
        // following that external metadata path.
        return walked;
    };
    let entries_visited = walked.entries_visited.saturating_add(index.entries);
    if entries_visited > WORKSPACE_RECEIPT_MAX_ENTRIES {
        return WorkspaceStateReceipt::unknown(
            WorkspaceReceiptStrategy::RepositoryWalk,
            WorkspaceReceiptUnknownReason::EntryLimit,
            entries_visited,
            walked.content_bytes_read.saturating_add(index.bytes_read),
        );
    }
    match git_workspace_receipt(root, git_program, deadline, &walked, index) {
        Ok(receipt) => receipt,
        // The anchored walk is already complete, so a missing, locked, broken,
        // or slow Git binary is an optimization miss rather than a tool error.
        Err(_) => walked,
    }
}

#[derive(Debug, Clone, Copy)]
struct GitIndexSummary {
    entries: usize,
    bytes_read: u64,
}

fn repository_index_entry_count(root: &Path) -> Option<GitIndexSummary> {
    let reader = AnchoredWorkspaceReader::new(root).ok()?;
    let mut index = match reader.open_regular_file(Path::new(".git/index")) {
        Ok(Some(index)) => index,
        Ok(None) => {
            return Some(GitIndexSummary {
                entries: 0,
                bytes_read: 0,
            });
        }
        Err(_) => return None,
    };
    let mut header = [0_u8; GIT_INDEX_HEADER_BYTES as usize];
    index.read_exact(&mut header).ok()?;
    if &header[..4] != b"DIRC" {
        return None;
    }
    let entries = usize::try_from(u32::from_be_bytes([
        header[8], header[9], header[10], header[11],
    ]))
    .ok()?;
    Some(GitIndexSummary {
        entries,
        bytes_read: GIT_INDEX_HEADER_BYTES,
    })
}

fn git_workspace_receipt(
    root: &Path,
    git_program: &OsStr,
    deadline: Instant,
    walked: &WorkspaceStateReceipt,
    index: GitIndexSummary,
) -> Result<WorkspaceStateReceipt, WorkspaceReceiptUnknownReason> {
    let status = run_git(
        root,
        git_program,
        &[
            OsStr::new("--no-optional-locks"),
            OsStr::new("-c"),
            OsStr::new("core.fsmonitor=false"),
            OsStr::new("-c"),
            OsStr::new("core.untrackedCache=false"),
            OsStr::new("status"),
            OsStr::new("--porcelain=v1"),
            OsStr::new("-z"),
            OsStr::new("--untracked-files=all"),
            OsStr::new("--ignore-submodules=all"),
            OsStr::new("--"),
            OsStr::new("."),
        ],
        deadline,
    )?;
    if !status.status.success() {
        return Err(WorkspaceReceiptUnknownReason::GitFailed);
    }
    let mut builder = ReceiptBuilder::new(WorkspaceReceiptStrategy::GitStatus, deadline);
    update_field(&mut builder.hasher, b"anchored-repository-walk-v1");
    update_field(&mut builder.hasher, walked.fingerprint.as_bytes());
    update_field(&mut builder.hasher, b"porcelain-v1-z");
    update_field(&mut builder.hasher, &status.bytes);
    let paths = porcelain_v1_paths(&status.bytes)?;
    if paths.len() > WORKSPACE_RECEIPT_MAX_ENTRIES {
        return Err(WorkspaceReceiptUnknownReason::EntryLimit);
    }
    builder.entries_visited = walked.entries_visited.saturating_add(index.entries);
    builder.content_bytes_read = walked.content_bytes_read.saturating_add(index.bytes_read);
    Ok(builder.finish())
}

struct GitCommandOutput {
    status: ExitStatus,
    bytes: Vec<u8>,
}

fn run_git(
    root: &Path,
    git_program: &OsStr,
    args: &[&OsStr],
    deadline: Instant,
) -> Result<GitCommandOutput, WorkspaceReceiptUnknownReason> {
    if Instant::now() >= deadline {
        return Err(WorkspaceReceiptUnknownReason::WallTimeLimit);
    }
    let mut command = Command::new(git_program);
    configure_git_environment(&mut command);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            WorkspaceReceiptUnknownReason::GitUnavailable
        } else {
            WorkspaceReceiptUnknownReason::GitFailed
        }
    })?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(WorkspaceReceiptUnknownReason::GitFailed);
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("haider-git-receipt-output".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let limit = u64::try_from(WORKSPACE_RECEIPT_GIT_OUTPUT_BYTES.saturating_add(1))
                .unwrap_or(u64::MAX);
            let result = stdout.take(limit).read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send(result);
        })
        .is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(WorkspaceReceiptUnknownReason::ReceiptWorkerFailed);
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(GIT_POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkspaceReceiptUnknownReason::WallTimeLimit);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkspaceReceiptUnknownReason::GitFailed);
            }
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let bytes = receiver
        .recv_timeout(remaining)
        .map_err(|_| WorkspaceReceiptUnknownReason::WallTimeLimit)?
        .map_err(|_| WorkspaceReceiptUnknownReason::GitFailed)?;
    if bytes.len() > WORKSPACE_RECEIPT_GIT_OUTPUT_BYTES {
        return Err(WorkspaceReceiptUnknownReason::GitOutputLimit);
    }
    Ok(GitCommandOutput { status, bytes })
}

fn configure_git_environment(command: &mut Command) {
    let path = std::env::var_os("PATH");
    #[cfg(windows)]
    let system_root = std::env::var_os("SYSTEMROOT");
    #[cfg(windows)]
    let path_ext = std::env::var_os("PATHEXT");
    command.env_clear();
    if let Some(path) = path {
        command.env("PATH", path);
    }
    #[cfg(windows)]
    if let Some(system_root) = system_root {
        command.env("SYSTEMROOT", system_root);
    }
    #[cfg(windows)]
    if let Some(path_ext) = path_ext {
        command.env("PATHEXT", path_ext);
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    #[cfg(unix)]
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    #[cfg(windows)]
    command.env("GIT_CONFIG_GLOBAL", "NUL");
}

fn porcelain_v1_paths(bytes: &[u8]) -> Result<Vec<PathBuf>, WorkspaceReceiptUnknownReason> {
    let mut records = bytes.split(|byte| *byte == 0).peekable();
    let mut paths = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() && records.peek().is_none() {
            break;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err(WorkspaceReceiptUnknownReason::GitStatusMalformed);
        }
        paths.push(git_path(&record[3..])?);
        if matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C') {
            let original = records
                .next()
                .ok_or(WorkspaceReceiptUnknownReason::GitStatusMalformed)?;
            paths.push(git_path(original)?);
        }
    }
    Ok(paths)
}

#[cfg(unix)]
fn git_path(bytes: &[u8]) -> Result<PathBuf, WorkspaceReceiptUnknownReason> {
    validate_relative_path(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
fn git_path(bytes: &[u8]) -> Result<PathBuf, WorkspaceReceiptUnknownReason> {
    let path = String::from_utf8(bytes.to_vec())
        .map_err(|_| WorkspaceReceiptUnknownReason::GitStatusMalformed)?;
    validate_relative_path(PathBuf::from(path))
}

fn validate_relative_path(path: PathBuf) -> Result<PathBuf, WorkspaceReceiptUnknownReason> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.components().next().is_some_and(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
        })
    {
        return Err(WorkspaceReceiptUnknownReason::PathEscaped);
    }
    Ok(path)
}

fn repository_walk_receipt(root: &Path, deadline: Instant) -> WorkspaceStateReceipt {
    let mut builder = ReceiptBuilder::new(WorkspaceReceiptStrategy::RepositoryWalk, deadline);
    builder.content_budget = builder
        .content_budget
        .saturating_sub(GIT_INDEX_HEADER_BYTES);
    let reader = match AnchoredWorkspaceReader::new(root) {
        Ok(reader) => reader,
        Err(reason) => {
            builder.mark_unknown(reason);
            return builder.finish();
        }
    };
    let mut walk_failure = None;
    let walked = walk_files(
        root,
        root,
        WalkOptions {
            respect_gitignore: true,
            respect_global_gitignore: false,
            include_hidden: true,
            stable_order: false,
            max_files: WORKSPACE_RECEIPT_MAX_ENTRIES,
            max_ignore_control_bytes: Some(
                WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES.saturating_sub(GIT_INDEX_HEADER_BYTES),
            ),
            deadline: Some(deadline),
        },
        |entry| {
            match entry {
                WalkEntry::Status {
                    truncated: true, ..
                } => {
                    walk_failure = Some(WorkspaceReceiptUnknownReason::EntryLimit);
                    return Ok(ControlFlow::Break(()));
                }
                WalkEntry::Status {
                    time_budget_reached: true,
                    ..
                } => {
                    walk_failure = Some(WorkspaceReceiptUnknownReason::WallTimeLimit);
                    return Ok(ControlFlow::Break(()));
                }
                WalkEntry::Status {
                    ignore_control_budget_reached: true,
                    ignore_control_bytes_read,
                    ..
                } => {
                    builder.content_bytes_read = builder
                        .content_bytes_read
                        .saturating_add(ignore_control_bytes_read);
                    walk_failure = Some(WorkspaceReceiptUnknownReason::ContentLimit);
                    return Ok(ControlFlow::Break(()));
                }
                WalkEntry::Status {
                    ignore_control_bytes_read,
                    ..
                } => {
                    builder.content_budget = builder
                        .content_budget
                        .saturating_sub(ignore_control_bytes_read);
                    builder.content_bytes_read = builder
                        .content_bytes_read
                        .saturating_add(ignore_control_bytes_read);
                }
                WalkEntry::File(relative) => {
                    if let Err(reason) = builder.hash_path(&reader, relative) {
                        walk_failure = Some(reason);
                        return Ok(ControlFlow::Break(()));
                    }
                }
                WalkEntry::Footprint(_) | WalkEntry::HiddenSensitiveFile(_) => {}
            }
            Ok(ControlFlow::Continue(()))
        },
    );
    match walked {
        Ok(outcome) => {
            builder.entries_visited = builder.entries_visited.max(outcome.entries_visited);
            if outcome.truncated {
                builder.mark_unknown(WorkspaceReceiptUnknownReason::EntryLimit);
            } else if outcome.time_budget_reached {
                builder.mark_unknown(WorkspaceReceiptUnknownReason::WallTimeLimit);
            } else if outcome.symlinks_visited > 0 {
                builder.mark_unknown(WorkspaceReceiptUnknownReason::SymlinkOrReparsePoint);
            } else if outcome.special_entries_visited > 0 {
                builder.mark_unknown(WorkspaceReceiptUnknownReason::DirectoryOrSpecialEntry);
            }
        }
        Err(_) => {
            builder.mark_unknown(WorkspaceReceiptUnknownReason::RepositoryWalkFailed);
            builder.counts_known = false;
        }
    }
    if let Some(reason) = walk_failure {
        builder.mark_unknown(reason);
    }
    builder.finish()
}

struct AnchoredWorkspaceReader {
    root: haider_platform::WorkspaceDirectory,
}

impl AnchoredWorkspaceReader {
    fn new(root: &Path) -> Result<Self, WorkspaceReceiptUnknownReason> {
        haider_platform::open_workspace_directory(root)
            .map(|root| Self { root })
            .map_err(|_| WorkspaceReceiptUnknownReason::EntryReadFailed)
    }

    #[cfg(unix)]
    fn open_regular_file(
        &self,
        relative: &Path,
    ) -> Result<Option<File>, WorkspaceReceiptUnknownReason> {
        use rustix::fs::{FileType, Mode, OFlags};

        let mut directory = haider_platform::duplicate_workspace_directory(&self.root)
            .map_err(|_| WorkspaceReceiptUnknownReason::EntryReadFailed)?;
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(component) = component else {
                return Err(WorkspaceReceiptUnknownReason::PathEscaped);
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
            directory = match rustix::fs::openat(&directory, component, flags, Mode::empty()) {
                Ok(opened) => opened,
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                    return Err(WorkspaceReceiptUnknownReason::SymlinkOrReparsePoint);
                }
                Err(_) => return Err(WorkspaceReceiptUnknownReason::EntryReadFailed),
            };
        }
        let metadata = rustix::fs::fstat(&directory)
            .map_err(|_| WorkspaceReceiptUnknownReason::EntryReadFailed)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
            return Err(WorkspaceReceiptUnknownReason::DirectoryOrSpecialEntry);
        }
        Ok(Some(File::from(directory)))
    }

    #[cfg(windows)]
    fn open_regular_file(
        &self,
        relative: &Path,
    ) -> Result<Option<File>, WorkspaceReceiptUnknownReason> {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

        let Some(leaf) = relative.file_name() else {
            return Err(WorkspaceReceiptUnknownReason::PathEscaped);
        };
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let root = haider_platform::duplicate_workspace_directory(&self.root)
            .map_err(|_| WorkspaceReceiptUnknownReason::EntryReadFailed)?;
        let parent = match haider_platform::open_workspace_subdirectory(root, parent, false) {
            Ok(parent) => parent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(WorkspaceReceiptUnknownReason::SymlinkOrReparsePoint),
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
            Err(_) => return Err(WorkspaceReceiptUnknownReason::EntryReadFailed),
        };
        let metadata = file
            .metadata()
            .map_err(|_| WorkspaceReceiptUnknownReason::EntryReadFailed)?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(WorkspaceReceiptUnknownReason::SymlinkOrReparsePoint);
        }
        if !metadata.is_file() {
            return Err(WorkspaceReceiptUnknownReason::DirectoryOrSpecialEntry);
        }
        Ok(Some(file))
    }
}

#[cfg(unix)]
fn hash_metadata(hasher: &mut blake3::Hasher, metadata: &Metadata) {
    hasher.update(&metadata.mode().to_be_bytes());
    hasher.update(&metadata.uid().to_be_bytes());
    hasher.update(&metadata.gid().to_be_bytes());
    hasher.update(&metadata.dev().to_be_bytes());
    hasher.update(&metadata.ino().to_be_bytes());
    hasher.update(&metadata.len().to_be_bytes());
    hasher.update(&metadata.mtime().to_be_bytes());
    hasher.update(&metadata.mtime_nsec().to_be_bytes());
    hasher.update(&metadata.ctime().to_be_bytes());
    hasher.update(&metadata.ctime_nsec().to_be_bytes());
}

#[cfg(windows)]
fn hash_metadata(hasher: &mut blake3::Hasher, metadata: &Metadata) {
    hasher.update(&metadata.len().to_be_bytes());
    hasher.update(&metadata.file_attributes().to_be_bytes());
    hasher.update(&metadata.creation_time().to_be_bytes());
    hasher.update(&metadata.last_write_time().to_be_bytes());
}

#[cfg(unix)]
fn metadata_matches(before: &Metadata, after: &Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

#[cfg(windows)]
fn metadata_matches(before: &Metadata, after: &Metadata) -> bool {
    before.len() == after.len()
        && before.file_attributes() == after.file_attributes()
        && before.creation_time() == after.creation_time()
        && before.last_write_time() == after.last_write_time()
}

#[derive(Debug)]
struct WorkspaceReceiptTrackerState {
    initialized: bool,
    baseline: Option<WorkspaceStateReceipt>,
    active: usize,
    ambiguity_epoch: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceReceiptTracker {
    state: Arc<Mutex<WorkspaceReceiptTrackerState>>,
}

impl Default for WorkspaceReceiptTracker {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(WorkspaceReceiptTrackerState {
                initialized: false,
                baseline: None,
                active: 0,
                ambiguity_epoch: 0,
            })),
        }
    }
}

impl WorkspaceReceiptTracker {
    pub(crate) fn needs_initial_receipt(&self) -> bool {
        !self.lock().initialized
    }

    pub(crate) fn install_initial_receipt(&self, receipt: WorkspaceStateReceipt) {
        let mut state = self.lock();
        if !state.initialized {
            state.initialized = true;
            state.baseline = Some(receipt);
        }
    }

    pub(crate) async fn begin_foreground_for_root(&self, root: PathBuf) -> WorkspaceReceiptLease {
        self.ensure_initial_receipt(root).await;
        self.begin_foreground()
    }

    pub(crate) async fn begin_detached_for_root(&self, root: PathBuf) -> WorkspaceReceiptLease {
        self.ensure_initial_receipt(root).await;
        self.begin_foreground()
    }

    async fn ensure_initial_receipt(&self, root: PathBuf) {
        if !self.needs_initial_receipt() {
            return;
        }
        let receipt =
            match tokio::task::spawn_blocking(move || workspace_state_receipt(&root)).await {
                Ok(receipt) => receipt,
                Err(_) => WorkspaceStateReceipt::unknown(
                    WorkspaceReceiptStrategy::NotEnumerated,
                    WorkspaceReceiptUnknownReason::ReceiptWorkerFailed,
                    0,
                    0,
                ),
            };
        self.install_initial_receipt(receipt);
    }

    pub(crate) fn begin_foreground(&self) -> WorkspaceReceiptLease {
        let mut state = self.lock();
        if !state.initialized {
            state.initialized = true;
        }
        let precise = state.active == 0 && state.baseline.is_some();
        let before = if precise {
            state.baseline.clone().unwrap_or_else(|| {
                WorkspaceStateReceipt::unknown(
                    WorkspaceReceiptStrategy::NotEnumerated,
                    WorkspaceReceiptUnknownReason::ReceiptWorkerFailed,
                    0,
                    0,
                )
            })
        } else {
            state.ambiguity_epoch = state.ambiguity_epoch.saturating_add(1);
            state.baseline = None;
            WorkspaceStateReceipt::unknown(
                WorkspaceReceiptStrategy::NotEnumerated,
                WorkspaceReceiptUnknownReason::ConcurrentOrInterleavedMutation,
                0,
                0,
            )
        };
        state.active = state.active.saturating_add(1);
        WorkspaceReceiptLease {
            tracker: self.clone(),
            before,
            epoch: state.ambiguity_epoch,
            precise,
            completed: false,
        }
    }

    pub(crate) fn invalidate(&self) {
        let mut state = self.lock();
        state.initialized = true;
        state.ambiguity_epoch = state.ambiguity_epoch.saturating_add(1);
        state.baseline = None;
    }

    fn finish(&self, epoch: u64, precise: bool, after: &WorkspaceStateReceipt) -> bool {
        let mut state = self.lock();
        state.active = state.active.saturating_sub(1);
        let comparison_precise = precise && epoch == state.ambiguity_epoch;
        if comparison_precise
            && state.active == 0
            && after.coverage == WorkspaceReceiptCoverage::Complete
        {
            state.baseline = Some(after.clone());
        } else if state.active == 0 {
            state.baseline = None;
        }
        comparison_precise
    }

    fn abandon(&self) {
        let mut state = self.lock();
        state.active = state.active.saturating_sub(1);
        state.ambiguity_epoch = state.ambiguity_epoch.saturating_add(1);
        state.baseline = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WorkspaceReceiptTrackerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct WorkspaceReceiptLease {
    tracker: WorkspaceReceiptTracker,
    before: WorkspaceStateReceipt,
    epoch: u64,
    precise: bool,
    completed: bool,
}

impl WorkspaceReceiptLease {
    pub(crate) fn finish(mut self, after: WorkspaceStateReceipt) -> Option<String> {
        let comparison_precise = self.tracker.finish(self.epoch, self.precise, &after);
        self.completed = true;
        compare_receipts(&self.before, &after, !comparison_precise)
    }
}

impl Drop for WorkspaceReceiptLease {
    fn drop(&mut self) {
        if !self.completed {
            self.tracker.abandon();
        }
    }
}

fn compare_receipts(
    before: &WorkspaceStateReceipt,
    after: &WorkspaceStateReceipt,
    forced_unknown: bool,
) -> Option<String> {
    if !forced_unknown
        && before.coverage == WorkspaceReceiptCoverage::Complete
        && after.coverage == WorkspaceReceiptCoverage::Complete
    {
        return (before.fingerprint != after.fingerprint).then(|| after.mutation_digest());
    }
    let reason = if forced_unknown {
        WorkspaceReceiptUnknownReason::ConcurrentOrInterleavedMutation
    } else {
        match after.coverage {
            WorkspaceReceiptCoverage::Unknown(reason) => reason,
            WorkspaceReceiptCoverage::Complete => match before.coverage {
                WorkspaceReceiptCoverage::Unknown(reason) => reason,
                WorkspaceReceiptCoverage::Complete => {
                    WorkspaceReceiptUnknownReason::ConcurrentOrInterleavedMutation
                }
            },
        }
    };
    let mut hasher = receipt_hasher(WorkspaceReceiptStrategy::NotEnumerated);
    update_field(&mut hasher, b"comparison-unknown-assumed-mutation");
    update_field(&mut hasher, reason.as_str().as_bytes());
    update_field(&mut hasher, before.fingerprint.as_bytes());
    update_field(&mut hasher, after.fingerprint.as_bytes());
    Some(format!(
        "{};coverage=unknown;reason={};assumed_mutation=true;before_counts={};after_counts={};before_entries={};after_entries={};before_bytes={};after_bytes={}",
        finalize(hasher),
        reason.as_str(),
        if before.counts_known {
            "known"
        } else {
            "unreported"
        },
        if after.counts_known {
            "known"
        } else {
            "unreported"
        },
        before.entries_visited,
        after.entries_visited,
        before.content_bytes_read,
        after.content_bytes_read,
    ))
}

#[cfg(test)]
#[path = "workspace_receipt_tests.rs"]
mod workspace_receipt_tests;
