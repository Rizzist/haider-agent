//! Privacy-bounded, crash-durable effect breadcrumbs.
//!
//! This journal is deliberately separate from the semantic effect journal:
//! it explains a process death, but never participates in recovery decisions.
//! A `start` is synced after the semantic `Dispatched` event and before the
//! risky executor is entered. A `complete` is synced after the semantic
//! outcome. Startup reports starts that have neither completion nor a prior
//! surfaced marker.

use haider_protocol::ids::{EffectId, RunId, SessionId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const EFFECT_DIAGNOSTIC_FILE: &str = "effect-diagnostics.jsonl";
pub(crate) const MAX_RECORD_BYTES: usize = 1_024;
pub(crate) const SEGMENT_BYTES: u64 = 1_048_576;
pub(crate) const RETAINED_SEGMENTS: usize = 4;

/// Build identity carried by every breadcrumb and ordinary panic marker.
pub const BUILD_UUID: &str = env!("HAIDER_BUILD_UUID");
pub const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

static PROCESS_STARTED_UNIX_MS: OnceLock<u64> = OnceLock::new();

/// Capture this as early as the executable can call into the daemon crate.
pub fn process_started_unix_ms() -> u64 {
    *PROCESS_STARTED_UNIX_MS.get_or_init(unix_time_ms)
}

#[derive(Debug, Clone)]
pub(crate) struct EffectBreadcrumb {
    pub(crate) session_id: SessionId,
    pub(crate) run_id: RunId,
    pub(crate) effect_id: EffectId,
    pub(crate) tool_name: String,
    pub(crate) workspace_root_digest: String,
    pub(crate) args_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PriorUnexpectedExit {
    pub(crate) build_version: String,
    pub(crate) build_uuid: String,
    pub(crate) pid: u32,
    pub(crate) process_started_unix_ms: u64,
    pub(crate) thread_name: String,
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    pub(crate) effect_id: String,
    pub(crate) tool_name: String,
    pub(crate) workspace_root_digest: String,
    pub(crate) args_digest: String,
    pub(crate) started_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DiagnosticKey {
    build_uuid: String,
    pid: u32,
    process_started_unix_ms: u64,
    session_id: String,
    run_id: String,
    effect_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiagnosticRecord {
    schema: String,
    phase: RecordPhase,
    build_version: String,
    build_uuid: String,
    pid: u32,
    process_started_unix_ms: u64,
    thread_name: String,
    session_id: String,
    run_id: String,
    effect_id: String,
    tool_name: String,
    workspace_root_digest: String,
    args_digest: String,
    timestamp_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordPhase {
    Start,
    Complete,
    PriorUnexpectedExit,
}

impl DiagnosticRecord {
    fn start(breadcrumb: &EffectBreadcrumb) -> Self {
        Self::new(RecordPhase::Start, breadcrumb)
    }

    fn complete(breadcrumb: &EffectBreadcrumb) -> Self {
        Self::new(RecordPhase::Complete, breadcrumb)
    }

    fn new(phase: RecordPhase, breadcrumb: &EffectBreadcrumb) -> Self {
        Self {
            schema: "haider.effect-diagnostic.v1".into(),
            phase,
            build_version: safe_identifier(BUILD_VERSION, 32),
            build_uuid: safe_identifier(BUILD_UUID, 64),
            pid: std::process::id(),
            process_started_unix_ms: process_started_unix_ms(),
            thread_name: safe_identifier(std::thread::current().name().unwrap_or("unnamed"), 64),
            session_id: safe_identifier(breadcrumb.session_id.as_str(), 128),
            run_id: safe_identifier(breadcrumb.run_id.as_str(), 128),
            effect_id: safe_identifier(breadcrumb.effect_id.as_str(), 192),
            tool_name: safe_identifier(&breadcrumb.tool_name, 64),
            workspace_root_digest: safe_digest(&breadcrumb.workspace_root_digest),
            args_digest: safe_digest(&breadcrumb.args_digest),
            timestamp_unix_ms: unix_time_ms(),
        }
    }

    fn surfaced(evidence: &PriorUnexpectedExit) -> Self {
        Self {
            schema: "haider.effect-diagnostic.v1".into(),
            phase: RecordPhase::PriorUnexpectedExit,
            build_version: evidence.build_version.clone(),
            build_uuid: evidence.build_uuid.clone(),
            pid: evidence.pid,
            process_started_unix_ms: evidence.process_started_unix_ms,
            thread_name: evidence.thread_name.clone(),
            session_id: evidence.session_id.clone(),
            run_id: evidence.run_id.clone(),
            effect_id: evidence.effect_id.clone(),
            tool_name: evidence.tool_name.clone(),
            workspace_root_digest: evidence.workspace_root_digest.clone(),
            args_digest: evidence.args_digest.clone(),
            timestamp_unix_ms: unix_time_ms(),
        }
    }

    fn key(&self) -> DiagnosticKey {
        DiagnosticKey {
            build_uuid: self.build_uuid.clone(),
            pid: self.pid,
            process_started_unix_ms: self.process_started_unix_ms,
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            effect_id: self.effect_id.clone(),
        }
    }

    fn evidence(&self) -> PriorUnexpectedExit {
        PriorUnexpectedExit {
            build_version: self.build_version.clone(),
            build_uuid: self.build_uuid.clone(),
            pid: self.pid,
            process_started_unix_ms: self.process_started_unix_ms,
            thread_name: self.thread_name.clone(),
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            effect_id: self.effect_id.clone(),
            tool_name: self.tool_name.clone(),
            workspace_root_digest: self.workspace_root_digest.clone(),
            args_digest: self.args_digest.clone(),
            started_unix_ms: self.timestamp_unix_ms,
        }
    }
}

struct WriterState {
    file: File,
    len: u64,
}

pub(crate) struct EffectDiagnostics {
    root: PathBuf,
    writer: Mutex<WriterState>,
}

impl EffectDiagnostics {
    /// Opens the bounded journal and returns every not-yet-surfaced orphan.
    /// The caller prints all evidence before [`Self::record_surfaced`] writes
    /// the one-shot marker, so a death in that gap causes a safe repeat rather
    /// than permanently suppressing evidence that was never shown.
    pub(crate) async fn open(
        store_dir: PathBuf,
    ) -> std::io::Result<(Arc<Self>, Vec<PriorUnexpectedExit>)> {
        tokio::task::spawn_blocking(move || Self::open_blocking(&store_dir))
            .await
            .map_err(|error| {
                std::io::Error::other(format!("diagnostic open task failed: {error}"))
            })?
    }

    fn open_blocking(store_dir: &Path) -> std::io::Result<(Arc<Self>, Vec<PriorUnexpectedExit>)> {
        std::fs::create_dir_all(store_dir)?;
        let starts = scan_unreported_starts(store_dir);
        let path = store_dir.join(EFFECT_DIAGNOSTIC_FILE);
        let file = open_owner_append(&path)?;
        let len = file.metadata()?.len();
        let diagnostics = Arc::new(Self {
            root: store_dir.to_path_buf(),
            writer: Mutex::new(WriterState { file, len }),
        });
        let mut evidence = starts
            .values()
            .map(DiagnosticRecord::evidence)
            .collect::<Vec<_>>();
        evidence.sort_by_key(|record| record.started_unix_ms);
        Ok((diagnostics, evidence))
    }

    pub(crate) async fn record_surfaced(
        self: &Arc<Self>,
        evidence: Vec<PriorUnexpectedExit>,
    ) -> std::io::Result<()> {
        if evidence.is_empty() {
            return Ok(());
        }
        let diagnostics = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut writer = diagnostics.lock_writer()?;
            for orphan in evidence {
                diagnostics.append_locked(&mut writer, &DiagnosticRecord::surfaced(&orphan))?;
            }
            writer.file.sync_data()
        })
        .await
        .map_err(|error| std::io::Error::other(format!("surface marker task failed: {error}")))?
    }

    pub(crate) async fn record_start(
        self: &Arc<Self>,
        breadcrumb: EffectBreadcrumb,
    ) -> std::io::Result<()> {
        let diagnostics = Arc::clone(self);
        let record = DiagnosticRecord::start(&breadcrumb);
        tokio::task::spawn_blocking(move || diagnostics.append_synced(&record))
            .await
            .map_err(|error| {
                std::io::Error::other(format!("start breadcrumb task failed: {error}"))
            })?
    }

    pub(crate) async fn record_completion(
        self: &Arc<Self>,
        breadcrumb: EffectBreadcrumb,
    ) -> std::io::Result<()> {
        let diagnostics = Arc::clone(self);
        let record = DiagnosticRecord::complete(&breadcrumb);
        tokio::task::spawn_blocking(move || diagnostics.append_synced(&record))
            .await
            .map_err(|error| {
                std::io::Error::other(format!("completion breadcrumb task failed: {error}"))
            })?
    }

    fn append_synced(&self, record: &DiagnosticRecord) -> std::io::Result<()> {
        let mut writer = self.lock_writer()?;
        self.append_locked(&mut writer, record)?;
        writer.file.sync_data()
    }

    fn append_locked(
        &self,
        writer: &mut WriterState,
        record: &DiagnosticRecord,
    ) -> std::io::Result<()> {
        let mut encoded = serde_json::to_vec(record)
            .map_err(|error| std::io::Error::other(format!("encode breadcrumb: {error}")))?;
        encoded.push(b'\n');
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(std::io::Error::other(format!(
                "diagnostic record is {} bytes; maximum is {MAX_RECORD_BYTES}",
                encoded.len()
            )));
        }
        if writer.len.saturating_add(encoded.len() as u64) > SEGMENT_BYTES {
            *writer = self.rotate()?;
        }
        writer.file.write_all(&encoded)?;
        writer.len = writer.len.saturating_add(encoded.len() as u64);
        Ok(())
    }

    fn rotate(&self) -> std::io::Result<WriterState> {
        for index in (1..RETAINED_SEGMENTS).rev() {
            let source = segment_path(&self.root, index - 1);
            let destination = segment_path(&self.root, index);
            if !source.exists() {
                continue;
            }
            if destination.exists() {
                match std::fs::remove_file(&destination) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            std::fs::rename(source, destination)?;
        }
        let file = open_owner_append(&segment_path(&self.root, 0))?;
        Ok(WriterState { file, len: 0 })
    }

    fn lock_writer(&self) -> std::io::Result<std::sync::MutexGuard<'_, WriterState>> {
        self.writer
            .lock()
            .map_err(|_| std::io::Error::other("effect diagnostic writer lock is poisoned"))
    }

    pub(crate) fn workspace_digest(path: &str) -> String {
        format!("blake3:{}", blake3::hash(path.as_bytes()).to_hex())
    }
}

fn scan_unreported_starts(store_dir: &Path) -> HashMap<DiagnosticKey, DiagnosticRecord> {
    let mut starts = HashMap::new();
    for index in (0..RETAINED_SEGMENTS).rev() {
        let path = segment_path(store_dir, index);
        let Ok(file) = File::open(path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(record) = serde_json::from_str::<DiagnosticRecord>(&line) else {
                continue;
            };
            if record.schema != "haider.effect-diagnostic.v1" {
                continue;
            }
            match record.phase {
                RecordPhase::Start => {
                    starts.insert(record.key(), record);
                }
                RecordPhase::Complete | RecordPhase::PriorUnexpectedExit => {
                    starts.remove(&record.key());
                }
            }
        }
    }
    starts
}

fn segment_path(root: &Path, index: usize) -> PathBuf {
    if index == 0 {
        root.join(EFFECT_DIAGNOSTIC_FILE)
    } else {
        root.join(format!("{EFFECT_DIAGNOSTIC_FILE}.{index}"))
    }
}

fn open_owner_append(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn safe_identifier(value: &str, maximum: usize) -> String {
    if value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@/".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("blake3:{}", blake3::hash(value.as_bytes()).to_hex())
    }
}

fn safe_digest(value: &str) -> String {
    if let Some(hex) = value.strip_prefix("blake3:")
        && hex.len() == 64
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return value.to_ascii_lowercase();
    }
    format!("blake3:{}", blake3::hash(value.as_bytes()).to_hex())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABRUPT_CHILD_STORE: &str = "HAIDER_DIAGNOSTIC_ABRUPT_CHILD_STORE";
    const ABRUPT_CHILD_EXIT: i32 = 86;

    fn breadcrumb(secret: &str) -> EffectBreadcrumb {
        EffectBreadcrumb {
            session_id: SessionId::new("session-diag"),
            run_id: RunId::new("run-diag"),
            effect_id: EffectId::new("effect-diag"),
            tool_name: "process_exec".into(),
            workspace_root_digest: EffectDiagnostics::workspace_digest("/private/workspace"),
            // Deliberately pass non-digest input: the writer must hash even a
            // malformed caller value rather than leak it.
            args_digest: secret.into(),
        }
    }

    #[tokio::test]
    async fn abrupt_exit_writer_child() {
        let Some(root) = std::env::var_os(ABRUPT_CHILD_STORE) else {
            return;
        };
        let (diagnostics, initial) = EffectDiagnostics::open(PathBuf::from(root))
            .await
            .unwrap_or_else(|error| panic!("open child journal: {error}"));
        assert!(initial.is_empty());
        diagnostics
            .record_start(breadcrumb("argument-secret"))
            .await
            .unwrap_or_else(|error| panic!("write child start: {error}"));
        // No stack unwinds and no destructor flushes. The parent can detect
        // the record only if record_start completed its own durable sync.
        std::process::exit(ABRUPT_CHILD_EXIT);
    }

    #[tokio::test]
    async fn start_without_completion_survives_non_unwinding_exit_and_is_reported_next_open() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("current test executable: {error}"));
        let status = std::process::Command::new(executable)
            .arg("--exact")
            .arg("diagnostics::tests::abrupt_exit_writer_child")
            .arg("--nocapture")
            .env(ABRUPT_CHILD_STORE, root.path())
            .status()
            .unwrap_or_else(|error| panic!("spawn abrupt child: {error}"));
        assert_eq!(status.code(), Some(ABRUPT_CHILD_EXIT));

        let journal = std::fs::read_to_string(root.path().join(EFFECT_DIAGNOSTIC_FILE))
            .unwrap_or_else(|error| panic!("inspect crash journal: {error}"));
        let records = journal
            .lines()
            .map(|line| {
                serde_json::from_str::<DiagnosticRecord>(line)
                    .unwrap_or_else(|error| panic!("decode produced breadcrumb: {error}"))
            })
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "the child must produce one real record");
        assert_eq!(records[0].phase, RecordPhase::Start);
        assert_eq!(records[0].effect_id, "effect-diag");

        let (second, evidence) = EffectDiagnostics::open(root.path().to_path_buf())
            .await
            .unwrap_or_else(|error| panic!("open after crash: {error}"));
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].effect_id, "effect-diag");
        second
            .record_surfaced(evidence)
            .await
            .unwrap_or_else(|error| panic!("mark evidence surfaced: {error}"));
        drop(second);

        let (_third, repeated) = EffectDiagnostics::open(root.path().to_path_buf())
            .await
            .unwrap_or_else(|error| panic!("open after report: {error}"));
        assert!(
            repeated.is_empty(),
            "an orphan marker de-duplicates reports"
        );
    }

    #[tokio::test]
    async fn completion_closes_start_and_sensitive_inputs_never_reach_disk() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let secret = "TOKEN-super-secret-tool-argument";
        let path_text = "/Users/alice/secret-client-project";
        let mut record = breadcrumb(secret);
        record.workspace_root_digest = EffectDiagnostics::workspace_digest(path_text);
        let (diagnostics, _) = EffectDiagnostics::open(root.path().to_path_buf())
            .await
            .unwrap_or_else(|error| panic!("open journal: {error}"));
        diagnostics
            .record_start(record.clone())
            .await
            .unwrap_or_else(|error| panic!("write start: {error}"));
        diagnostics
            .record_completion(record)
            .await
            .unwrap_or_else(|error| panic!("write completion: {error}"));
        drop(diagnostics);

        let bytes = std::fs::read(root.path().join(EFFECT_DIAGNOSTIC_FILE))
            .unwrap_or_else(|error| panic!("read journal: {error}"));
        let text = String::from_utf8(bytes).unwrap_or_else(|error| panic!("utf8: {error}"));
        assert!(!text.contains(secret));
        assert!(!text.contains(path_text));
        assert!(text.lines().all(|line| line.len() < MAX_RECORD_BYTES));

        let (_next, evidence) = EffectDiagnostics::open(root.path().to_path_buf())
            .await
            .unwrap_or_else(|error| panic!("reopen journal: {error}"));
        assert!(evidence.is_empty());
    }
}
