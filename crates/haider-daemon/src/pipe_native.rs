//! Best-effort daemon maintenance for native session JSONL sidecars.
//!
//! The journal remains authoritative. A sidecar failure is reported and the
//! session is left unreconciled so its next committed append retries from the
//! last self-describing line (or rebuilds a corrupt file).
//! Readers compute `covered_through` as the maximum row `seq` or coverage
//! value encountered while reading forward. EOF proves the reader is at head
//! only when that value equals the roster/status `head_seq`.

use haider_core::{SqliteStoreHandle, StoreHandle};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::SessionId;
use haider_protocol::pipe::sidecar_row_line;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const RECONCILE_PAGE_ENVELOPES: usize = 1_024;
const RECONCILE_PAGE_BYTES: usize = 4 * 1_024 * 1_024;
const COVERAGE_COALESCE_ENVELOPES: u64 = 256;
const TAIL_SCAN_BYTES: usize = 8 * 1_024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

const SIDECAR_MAGIC: &str = "haider.session.jsonl";
// V2 adds coverage line kinds and `(seq, ordinal)` row identity with optional
// branches. Readers are forward-compatible by ignoring unknown row keys and
// unknown line kinds.
const SIDECAR_VERSION: u64 = 2;

#[derive(Debug, Clone, Copy)]
struct SidecarCursor {
    /// Cursor represented by the last persisted row or coverage line.
    seq: u64,
    /// Highest envelope processed in this daemon lifetime. This may be ahead
    /// of `seq` while non-projecting hot batches are being coalesced.
    pending_seq: u64,
    generation: u64,
}

#[derive(Serialize, Deserialize)]
struct SidecarHeader {
    pipe: String,
    version: u64,
    session_id: String,
    generation: u64,
}

/// A durable proof that all journal envelopes through `coverage` were
/// inspected, including envelopes which project no transcript row.
#[derive(Serialize)]
struct SidecarCoverage {
    coverage: u64,
    generation: u64,
}

#[derive(Debug)]
pub(crate) struct PipeNativeError(String);

impl fmt::Display for PipeNativeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<std::io::Error> for PipeNativeError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

/// Profile-scoped reconciliation state. Session actors remain the actual
/// single writers; this map records which actors have completed their lazy
/// first-touch reconciliation in this daemon lifetime.
pub(crate) struct PipeNativeWriter {
    pipe_dir: PathBuf,
    reconciled: Mutex<HashMap<SessionId, SidecarCursor>>,
    dirty: Mutex<HashSet<SessionId>>,
}

impl PipeNativeWriter {
    pub(crate) fn new(store_root: &Path) -> Self {
        Self {
            pipe_dir: store_root.join("pipe"),
            reconciled: Mutex::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
        }
    }

    /// Maintains one session after its journal batch has committed. Ordinary
    /// appends are intentionally not fsynced: the journal owns durability and
    /// boot reconciliation heals a lost or torn sidecar tail. Errors are
    /// returned only for observation; callers must never fail the append.
    pub(crate) async fn maintain(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        committed: &[RawEnvelope],
    ) -> Result<(), PipeNativeError> {
        let result = self.maintain_inner(store, session_id, committed).await;
        if result.is_err() {
            self.reconciled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(session_id);
            self.dirty
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(session_id.clone());
        }
        result
    }

    async fn maintain_inner(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        committed: &[RawEnvelope],
    ) -> Result<(), PipeNativeError> {
        let path = self.sidecar_path(session_id)?;
        let known_cursor = self
            .reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .copied();

        let dirty = self
            .dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id);
        let cursor = if let Some(cursor) = known_cursor {
            let (data, next_cursor) = render_hot_batch(committed, cursor)?;
            append_once(path, data).await?;
            next_cursor
        } else {
            let state = inspect_sidecar(path.clone(), session_id.clone()).await?;
            if dirty {
                self.rebuild(store, session_id, path, state.generation())
                    .await?
            } else {
                match state {
                    SidecarState::Missing => self.rebuild(store, session_id, path, 0).await?,
                    SidecarState::Corrupt { generation } => {
                        self.rebuild(store, session_id, path, generation).await?
                    }
                    SidecarState::Ready(cursor) => {
                        let latest_seq = store.latest_seq(session_id).await.map_err(|error| {
                            PipeNativeError(format!("journal head inspection failed: {error:?}"))
                        })?;
                        if cursor.seq > latest_seq {
                            self.rebuild(store, session_id, path, cursor.generation)
                                .await?
                        } else {
                            self.reconcile_from(store, session_id, path, cursor).await?
                        }
                    }
                }
            }
        };

        self.reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.clone(), cursor);
        self.dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        Ok(())
    }

    /// Reconciles pages inline to preserve append ordering. Each page is
    /// rendered and written before the next is read, bounding memory at the
    /// cost of keeping the owning session actor occupied during cold catch-up.
    async fn reconcile_from(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        path: PathBuf,
        cursor: SidecarCursor,
    ) -> Result<SidecarCursor, PipeNativeError> {
        let mut read_cursor = cursor.seq;
        let mut append_file = None;
        loop {
            let page = store
                .read_page(
                    session_id,
                    read_cursor,
                    RECONCILE_PAGE_ENVELOPES,
                    RECONCILE_PAGE_BYTES,
                )
                .await
                .map_err(|error| {
                    PipeNativeError(format!("journal reconciliation failed: {error:?}"))
                })?;
            let Some(last) = page.last() else {
                break;
            };
            read_cursor = last.seq;
            let chunk = render_rows_after(&page, cursor.seq);
            if !chunk.is_empty() {
                let file = match append_file.take() {
                    Some(file) => file,
                    None => open_append(path.clone()).await?,
                };
                append_file = Some(write_open(file, chunk).await?);
            }
        }
        let file = match append_file.take() {
            Some(file) => file,
            None => open_append(path).await?,
        };
        let file = write_open(file, coverage_line(read_cursor, cursor.generation)?).await?;
        // Reconciliation is a repair path, so retain an explicit sync here.
        sync_open(file).await?;
        Ok(SidecarCursor {
            seq: read_cursor,
            pending_seq: read_cursor,
            ..cursor
        })
    }

    async fn rebuild(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        path: PathBuf,
        generation: u64,
    ) -> Result<SidecarCursor, PipeNativeError> {
        let (mut file, temp_path) = create_temp(path.clone()).await?;
        let generation = generation
            .checked_add(1)
            .ok_or_else(|| PipeNativeError("sidecar generation exhausted".into()))?;
        let mut header = serde_json::to_string(&SidecarHeader {
            pipe: SIDECAR_MAGIC.to_owned(),
            version: SIDECAR_VERSION,
            session_id: session_id.as_str().to_owned(),
            generation,
        })
        .map_err(|error| {
            PipeNativeError(format!("sidecar header serialization failed: {error}"))
        })?;
        header.push('\n');
        file = write_temp(file, header).await?;
        let mut read_cursor = 0;
        loop {
            let page = store
                .read_page(
                    session_id,
                    read_cursor,
                    RECONCILE_PAGE_ENVELOPES,
                    RECONCILE_PAGE_BYTES,
                )
                .await
                .map_err(|error| PipeNativeError(format!("journal rebuild failed: {error:?}")))?;
            let Some(last) = page.last() else {
                break;
            };
            read_cursor = last.seq;
            let chunk = render_rows_after(&page, 0);
            file = write_temp(file, chunk).await?;
        }
        file = write_temp(file, coverage_line(read_cursor, generation)?).await?;
        finish_temp(file, temp_path, path).await?;
        Ok(SidecarCursor {
            seq: read_cursor,
            pending_seq: read_cursor,
            generation,
        })
    }

    pub(crate) fn sidecar_path(&self, session_id: &SessionId) -> Result<PathBuf, PipeNativeError> {
        let id = session_id.as_str();
        // Production session ids are generated by `random_id("session")`
        // and use only this filename-safe alphabet. Keep the boundary strict
        // for directly seeded/test journals too, so joining can never escape
        // the profile's pipe directory.
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || matches!(id, "." | "..")
        {
            return Err(PipeNativeError(format!(
                "session id is not a safe sidecar filename: {id:?}"
            )));
        }
        let pipe_dir = if self.pipe_dir.is_absolute() {
            self.pipe_dir.clone()
        } else {
            std::env::current_dir()
                .map_err(|error| {
                    PipeNativeError(format!("cannot resolve absolute sidecar path: {error}"))
                })?
                .join(&self.pipe_dir)
        };
        Ok(pipe_dir.join(format!("{id}.pipe")))
    }
}

fn ordered_after(envelopes: &[RawEnvelope], cursor: u64) -> Vec<&RawEnvelope> {
    let mut ordered: Vec<&RawEnvelope> = envelopes
        .iter()
        .filter(|envelope| envelope.seq > cursor)
        .collect();
    ordered.sort_by_key(|envelope| envelope.seq);
    ordered
}

fn render_rows_after(envelopes: &[RawEnvelope], cursor: u64) -> String {
    let mut data = String::new();
    for envelope in ordered_after(envelopes, cursor) {
        if let Some(line) = sidecar_row_line(envelope) {
            data.push_str(&line);
            data.push('\n');
        }
    }
    data
}

fn coverage_line(coverage: u64, generation: u64) -> Result<String, PipeNativeError> {
    let mut line = serde_json::to_string(&SidecarCoverage {
        coverage,
        generation,
    })
    .map_err(|error| PipeNativeError(format!("sidecar coverage serialization failed: {error}")))?;
    line.push('\n');
    Ok(line)
}

/// Render an ordinary committed batch. A row-producing batch gets exactly
/// one trailing watermark. Non-projecting batches coalesce until 256 newly
/// covered envelopes can be represented by one watermark.
fn render_hot_batch(
    envelopes: &[RawEnvelope],
    cursor: SidecarCursor,
) -> Result<(String, SidecarCursor), PipeNativeError> {
    let ordered = ordered_after(envelopes, cursor.pending_seq);
    let Some(last) = ordered.last() else {
        return Ok((String::new(), cursor));
    };
    let pending_seq = last.seq;
    let mut data = String::new();
    let mut produced_row = false;
    for envelope in ordered {
        if let Some(line) = sidecar_row_line(envelope) {
            data.push_str(&line);
            data.push('\n');
            produced_row = true;
        }
    }
    let should_cover =
        produced_row || pending_seq.saturating_sub(cursor.seq) >= COVERAGE_COALESCE_ENVELOPES;
    let seq = if should_cover {
        data.push_str(&coverage_line(pending_seq, cursor.generation)?);
        pending_seq
    } else {
        cursor.seq
    };
    Ok((
        data,
        SidecarCursor {
            seq,
            pending_seq,
            ..cursor
        },
    ))
}

enum SidecarState {
    Missing,
    Ready(SidecarCursor),
    Corrupt { generation: u64 },
}

impl SidecarState {
    fn generation(&self) -> u64 {
        match self {
            Self::Ready(cursor) => cursor.generation,
            Self::Corrupt { generation } => *generation,
            Self::Missing => 0,
        }
    }
}

async fn inspect_sidecar(
    path: PathBuf,
    session_id: SessionId,
) -> Result<SidecarState, PipeNativeError> {
    tokio::task::spawn_blocking(move || inspect_sidecar_blocking(&path, &session_id))
        .await
        .map_err(|error| PipeNativeError(format!("sidecar inspection task failed: {error}")))?
}

fn inspect_sidecar_blocking(
    path: &Path,
    session_id: &SessionId,
) -> Result<SidecarState, PipeNativeError> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SidecarState::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    let mut len = file.metadata()?.len();
    if len == 0 {
        return Ok(SidecarState::Corrupt { generation: 0 });
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut final_byte = [0_u8; 1];
    file.read_exact(&mut final_byte)?;
    if final_byte[0] != b'\n' {
        len = find_previous_newline(&mut file, len)?.map_or(0, |position| position + 1);
        file.set_len(len)?;
        file.sync_data()?;
    }
    if len == 0 {
        return Ok(SidecarState::Corrupt { generation: 0 });
    }

    file.seek(SeekFrom::Start(0))?;
    let mut header_line = String::new();
    let header_len = BufReader::new(&mut file).read_line(&mut header_line)? as u64;
    let Ok(header) = serde_json::from_str::<SidecarHeader>(header_line.trim_end_matches('\n'))
    else {
        return Ok(SidecarState::Corrupt { generation: 0 });
    };
    if header.pipe != SIDECAR_MAGIC
        || header.version != SIDECAR_VERSION
        || header.session_id != session_id.as_str()
        || header.generation == 0
    {
        return Ok(SidecarState::Corrupt {
            generation: header.generation,
        });
    }
    if len == header_len {
        return Ok(SidecarState::Ready(SidecarCursor {
            seq: 0,
            pending_seq: 0,
            generation: header.generation,
        }));
    }

    let line_start = find_previous_newline(&mut file, len - 1)?.map_or(0, |position| position + 1);
    let line_len = usize::try_from((len - 1) - line_start)
        .map_err(|_| PipeNativeError("sidecar tail is too large to inspect".into()))?;
    let mut bytes = vec![0_u8; line_len];
    file.seek(SeekFrom::Start(line_start))?;
    file.read_exact(&mut bytes)?;
    let Ok(line) = std::str::from_utf8(&bytes) else {
        return Ok(SidecarState::Corrupt {
            generation: header.generation,
        });
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(SidecarState::Corrupt {
            generation: header.generation,
        });
    };
    // The tail must be either a real row or a coverage watermark, not any JSON
    // object that happens to carry an in-range cursor.
    let row_shaped = value
        .get("role")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|role| matches!(role, "user" | "assistant" | "error" | "tool"))
        && value
            .get("at_ms")
            .and_then(serde_json::Value::as_u64)
            .is_some();
    let coverage_shaped = value
        .get("coverage")
        .and_then(serde_json::Value::as_u64)
        .is_some()
        && value
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            .is_some();
    if !row_shaped && !coverage_shaped {
        return Ok(SidecarState::Corrupt {
            generation: header.generation,
        });
    }
    let Some(seq) = value
        .get(if row_shaped { "seq" } else { "coverage" })
        .and_then(serde_json::Value::as_u64)
    else {
        return Ok(SidecarState::Corrupt {
            generation: header.generation,
        });
    };
    Ok(SidecarState::Ready(SidecarCursor {
        seq,
        pending_seq: seq,
        generation: header.generation,
    }))
}

fn find_previous_newline(file: &mut File, exclusive_end: u64) -> std::io::Result<Option<u64>> {
    let mut end = exclusive_end;
    let mut buffer = vec![0_u8; TAIL_SCAN_BYTES];
    while end > 0 {
        let start = end.saturating_sub(TAIL_SCAN_BYTES as u64);
        let len = usize::try_from(end - start).unwrap_or(TAIL_SCAN_BYTES);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..len])?;
        if let Some(index) = buffer[..len].iter().rposition(|byte| *byte == b'\n') {
            return Ok(Some(start + index as u64));
        }
        end = start;
    }
    Ok(None)
}

async fn append_once(path: PathBuf, data: String) -> Result<(), PipeNativeError> {
    if data.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || {
        let mut file = OpenOptions::new().append(true).open(path)?;
        file.write_all(data.as_bytes())?;
        Ok(())
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar append task failed: {error}")))?
}

async fn open_append(path: PathBuf) -> Result<File, PipeNativeError> {
    tokio::task::spawn_blocking(move || {
        OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(PipeNativeError::from)
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar append-open task failed: {error}")))?
}

async fn write_open(mut file: File, data: String) -> Result<File, PipeNativeError> {
    tokio::task::spawn_blocking(move || {
        file.write_all(data.as_bytes())?;
        Ok(file)
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar page append task failed: {error}")))?
}

async fn sync_open(file: File) -> Result<(), PipeNativeError> {
    tokio::task::spawn_blocking(move || file.sync_data().map_err(PipeNativeError::from))
        .await
        .map_err(|error| PipeNativeError(format!("sidecar append sync task failed: {error}")))?
}

async fn create_temp(path: PathBuf) -> Result<(File, PathBuf), PipeNativeError> {
    tokio::task::spawn_blocking(move || {
        let parent = path
            .parent()
            .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
        std::fs::create_dir_all(parent)?;
        loop {
            let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let temp_path =
                parent.join(format!(".pipe-rebuild-{}-{suffix}.tmp", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) => return Ok((file, temp_path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar temp creation task failed: {error}")))?
}

async fn write_temp(mut file: File, data: String) -> Result<File, PipeNativeError> {
    tokio::task::spawn_blocking(move || {
        file.write_all(data.as_bytes())?;
        Ok(file)
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar rebuild write task failed: {error}")))?
}

async fn finish_temp(file: File, temp_path: PathBuf, path: PathBuf) -> Result<(), PipeNativeError> {
    tokio::task::spawn_blocking(move || {
        file.sync_data()?;
        std::fs::rename(&temp_path, &path).inspect_err(|_error| {
            let _ = std::fs::remove_file(&temp_path);
        })?;
        let parent = path
            .parent()
            .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar rebuild finalize task failed: {error}")))?
}
