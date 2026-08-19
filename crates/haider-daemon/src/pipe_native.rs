//! Best-effort daemon maintenance for native instruct-pipe sidecars.
//!
//! The journal remains authoritative. A sidecar failure is reported and the
//! session is left unreconciled so its next committed append retries from the
//! last self-describing line (or rebuilds a corrupt file).

use haider_core::SqliteStoreHandle;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::SessionId;
use haider_protocol::pipe::pipe_body_line;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const RECONCILE_PAGE_ENVELOPES: usize = 1_024;
const RECONCILE_PAGE_BYTES: usize = 4 * 1_024 * 1_024;
const TAIL_SCAN_BYTES: usize = 8 * 1_024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

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
    reconciled: Mutex<HashMap<SessionId, u64>>,
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

    /// Maintains one session after its journal batch has committed. Errors are
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
            let (data, cursor) = render_after(committed, cursor);
            append_once(path, data).await?;
            cursor
        } else if dirty {
            self.rebuild(store, session_id, path).await?
        } else {
            match inspect_sidecar(path.clone()).await? {
                SidecarState::Corrupt => self.rebuild(store, session_id, path).await?,
                SidecarState::Ready(cursor) => {
                    self.reconcile_from(store, session_id, path, cursor).await?
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

    async fn reconcile_from(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        path: PathBuf,
        cursor: u64,
    ) -> Result<u64, PipeNativeError> {
        let mut read_cursor = cursor;
        let mut line_cursor = cursor;
        let mut data = String::new();
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
            let (chunk, next_cursor) = render_after(&page, line_cursor);
            data.push_str(&chunk);
            line_cursor = next_cursor;
        }
        append_once(path, data).await?;
        Ok(line_cursor)
    }

    async fn rebuild(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        path: PathBuf,
    ) -> Result<u64, PipeNativeError> {
        let (mut file, temp_path) = create_temp(path.clone()).await?;
        let mut read_cursor = 0;
        let mut line_cursor = 0;
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
            let (chunk, next_cursor) = render_after(&page, line_cursor);
            line_cursor = next_cursor;
            file = write_temp(file, chunk).await?;
        }
        finish_temp(file, temp_path, path).await?;
        Ok(line_cursor)
    }

    fn sidecar_path(&self, session_id: &SessionId) -> Result<PathBuf, PipeNativeError> {
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
        Ok(self.pipe_dir.join(format!("{id}.pipe")))
    }
}

fn render_after(envelopes: &[RawEnvelope], cursor: u64) -> (String, u64) {
    let mut ordered: Vec<&RawEnvelope> = envelopes
        .iter()
        .filter(|envelope| envelope.seq > cursor)
        .collect();
    ordered.sort_by_key(|envelope| envelope.seq);
    let mut data = String::new();
    let mut line_cursor = cursor;
    for envelope in ordered {
        if let Some(line) = pipe_body_line(envelope) {
            data.push_str(&line);
            data.push('\n');
            line_cursor = envelope.seq;
        }
    }
    (data, line_cursor)
}

enum SidecarState {
    Ready(u64),
    Corrupt,
}

async fn inspect_sidecar(path: PathBuf) -> Result<SidecarState, PipeNativeError> {
    tokio::task::spawn_blocking(move || inspect_sidecar_blocking(&path))
        .await
        .map_err(|error| PipeNativeError(format!("sidecar inspection task failed: {error}")))?
}

fn inspect_sidecar_blocking(path: &Path) -> Result<SidecarState, PipeNativeError> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SidecarState::Ready(0));
        }
        Err(error) => return Err(error.into()),
    };
    let mut len = file.metadata()?.len();
    if len == 0 {
        return Ok(SidecarState::Ready(0));
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
        return Ok(SidecarState::Ready(0));
    }

    let line_start = find_previous_newline(&mut file, len - 1)?.map_or(0, |position| position + 1);
    let line_len = usize::try_from((len - 1) - line_start)
        .map_err(|_| PipeNativeError("sidecar tail is too large to inspect".into()))?;
    let mut bytes = vec![0_u8; line_len];
    file.seek(SeekFrom::Start(line_start))?;
    file.read_exact(&mut bytes)?;
    let Ok(line) = std::str::from_utf8(&bytes) else {
        return Ok(SidecarState::Corrupt);
    };
    let Some(seq) = line
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse::<u64>().ok())
    else {
        return Ok(SidecarState::Corrupt);
    };
    Ok(SidecarState::Ready(seq))
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
        let parent = path
            .parent()
            .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
        std::fs::create_dir_all(parent)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(data.as_bytes())?;
        file.sync_data()?;
        Ok(())
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar append task failed: {error}")))?
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
        Ok(())
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar rebuild finalize task failed: {error}")))?
}
