//! Best-effort daemon maintenance for native session JSONL sidecars.
//!
//! The journal remains authoritative. A sidecar failure is reported and the
//! session is left unreconciled so its next committed append retries from the
//! last self-describing line (or rebuilds a corrupt file).
//! Readers start at `<session>.pipe`, compute `covered_through` as the maximum
//! row `seq` or coverage value encountered while reading forward, and follow
//! every `segment_end: "sealed"` terminator's relative `successor` filename.
//! EOF of a sealed segment never proves anything about journal head, even when
//! its coverage happens to equal the roster/status `head_seq`; the successor
//! must be opened. Only EOF of the final, unterminated segment proves at-head,
//! and only when `covered_through == head_seq`.
//!
//! Sealed segments are immutable within one v4 generation. A future sidecar
//! version bump rebuilds the complete reachable chain from the authoritative
//! journal and atomically replaces the stable root last; old-generation
//! successor files are unreachable historical debris, never mixed into the
//! new chain. First-touch reconciliation follows the live root's successor
//! pointers to prove the complete reachable set before sweeping that debris;
//! an uncertain chain always leaves every file untouched.

use haider_core::{SqliteStoreHandle, StoreHandle};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::SessionId;
use haider_protocol::pipe::TranscriptProjector;
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
const JOIN_PREWARM_ENVELOPES: u64 = 1_024;
const COVERAGE_COALESCE_ENVELOPES: u64 = 256;
const TAIL_SCAN_BYTES: usize = 8 * 1_024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

const SIDECAR_MAGIC: &str = "haider.session.jsonl";
// V2 added coverage lines and `(seq, ordinal)` row identity. V3 guarantees
// cold tool preview projection. V4 adds sealed reasoning, compaction boundary
// rows, and physical segments. V5 carries no new row shape at all — it exists
// solely to REWRITE what v4 already wrote. V6 adds typed tool status and
// rewrites older rows so rejected/conflicted outcomes stop looking successful.
//
// v0.0.940 stopped marking reasoning-bearing rows `compat` (a row whose loss
// costs data is not redundant), but a producer-side contract fix does not
// reach a durable artefact unless something forces a rewrite. With the
// version unchanged the rebuild trigger `header.version != SIDECAR_VERSION`
// never fired, so every file on disk kept v4's flags and 93 reasoning rows
// went on advertising themselves as droppable. The fix and the fix's REACH
// are two different questions, and shipping only answers the first.
//
// Every bump intentionally forces existing at-head files (including every
// sealed segment) through a journal rebuild so old projections cannot remain
// silently "current" at EOF.
const SIDECAR_VERSION: u64 = 6;

#[derive(Debug, Clone, Copy)]
struct SidecarCursor {
    /// Cursor represented by the last persisted row or coverage line.
    seq: u64,
    /// Highest envelope processed in this daemon lifetime. This may be ahead
    /// of `seq` while non-projecting hot batches are being coalesced.
    pending_seq: u64,
    generation: u64,
    segment: u64,
}

struct ReconciledSidecar {
    cursor: SidecarCursor,
    projector: TranscriptProjector,
    base_path: PathBuf,
    /// Append handle kept open across hot batches within one reconciled
    /// lifetime, so steady-state appends stop paying open/close per batch.
    /// Any maintenance error evicts the entry, dropping (closing) the handle;
    /// the next touch re-inspects and reopens from the durable tail.
    file: File,
}

#[derive(Serialize, Deserialize)]
struct SidecarHeader {
    pipe: String,
    version: u64,
    session_id: String,
    generation: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    segment: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    starts_after: u64,
}

/// A durable proof that all journal envelopes through `coverage` were
/// inspected, including envelopes which project no transcript row.
#[derive(Serialize)]
struct SidecarCoverage {
    coverage: u64,
    generation: u64,
}

/// Final row of an immutable segment. It is also a normal coverage value so
/// the reader's max-seq rule remains uniform while `segment_end` prevents EOF
/// from being mistaken for the live head.
#[derive(Serialize, Deserialize)]
struct SidecarSegmentEnd {
    segment_end: String,
    coverage: u64,
    generation: u64,
    successor: String,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
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

/// Profile-scoped reconciliation state. Each session actor owns one writer
/// task, which remains that session's single sidecar writer; this map records
/// which tasks completed lazy first-touch reconciliation in this daemon life.
pub(crate) struct PipeNativeWriter {
    pipe_dir: PathBuf,
    reconciled: Mutex<HashMap<SessionId, ReconciledSidecar>>,
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

    /// Forgets an in-memory cursor after an asynchronous writer exits before
    /// draining its post-commit queue. The next touch must reconcile from the
    /// journal instead of advancing from a cursor that may have missed a batch.
    pub(crate) fn invalidate(&self, session_id: &SessionId) {
        self.reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        self.dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.clone());
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
            self.invalidate(session_id);
        }
        result
    }

    /// Cold-boot reconciliation using the journal page already read by turn
    /// recovery. Only a suffix committed after that sealed boot page is read
    /// from SQLite; missing/corrupt sidecars rebuild from the shared bytes.
    pub(crate) async fn maintain_from_boot_journal(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        journal: &[RawEnvelope],
    ) -> Result<(), PipeNativeError> {
        let path = self.sidecar_path(session_id)?;
        let state = inspect_sidecar(path.clone(), session_id.clone()).await?;
        let boot_head = journal.last().map_or(0, |envelope| envelope.seq);
        let latest_seq = store.latest_seq(session_id).await.map_err(|error| {
            PipeNativeError(format!("journal head inspection failed: {error:?}"))
        })?;
        let reconciled = match state {
            SidecarState::Ready(cursor) if cursor.seq <= boot_head && cursor.seq <= latest_seq => {
                self.reconcile_from_boot(store, session_id, path, cursor, journal)
                    .await?
            }
            SidecarState::Ready(cursor) => {
                self.rebuild_from_boot(store, session_id, path, cursor.generation, journal)
                    .await?
            }
            SidecarState::Missing => {
                self.rebuild_from_boot(store, session_id, path, 0, journal)
                    .await?
            }
            SidecarState::Corrupt { generation } => {
                self.rebuild_from_boot(store, session_id, path, generation, journal)
                    .await?
            }
        };
        sweep_orphan_segments_best_effort(&reconciled.base_path, session_id).await;
        self.reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.clone(), reconciled);
        self.dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        Ok(())
    }

    async fn reconcile_from_boot(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        path: PathBuf,
        cursor: SidecarCursor,
        journal: &[RawEnvelope],
    ) -> Result<ReconciledSidecar, PipeNativeError> {
        let mut projector = TranscriptProjector::default();
        let prewarm_start = cursor.seq.saturating_sub(JOIN_PREWARM_ENVELOPES);
        for envelope in ordered_after(journal, prewarm_start) {
            if envelope.seq > cursor.seq {
                break;
            }
            projector.prewarm(envelope);
        }
        let active_path = segment_path(&path, cursor.generation, cursor.segment)?;
        let mut file = open_append(active_path).await?;
        let mut segment = cursor.segment;
        let mut sealed_root = None;
        let boot_tail = render_rows_after(journal, cursor.seq, &mut projector);
        if !boot_tail.is_empty() {
            (file, segment) = write_segmented_open(
                file,
                boot_tail,
                &path,
                session_id,
                cursor.generation,
                segment,
                false,
                &mut sealed_root,
            )
            .await?;
        }
        let mut read_cursor = journal
            .last()
            .map_or(cursor.seq, |envelope| envelope.seq.max(cursor.seq));
        (file, segment) = append_store_suffix(
            store,
            session_id,
            path.clone(),
            file,
            segment,
            cursor.generation,
            &mut projector,
            &mut read_cursor,
        )
        .await?;
        let trailing = render_projected_rows(projector.flush_unresolved_tools());
        if !trailing.is_empty() {
            (file, segment) = write_segmented_open(
                file,
                trailing,
                &path,
                session_id,
                cursor.generation,
                segment,
                false,
                &mut sealed_root,
            )
            .await?;
        }
        let covered = projector
            .blocked_seq()
            .map_or(read_cursor, |seq| seq.saturating_sub(1));
        file = write_open(file, coverage_line(covered, cursor.generation)?).await?;
        let file = sync_open(file).await?;
        Ok(ReconciledSidecar {
            cursor: SidecarCursor {
                seq: covered,
                pending_seq: read_cursor,
                generation: cursor.generation,
                segment,
            },
            projector,
            base_path: path,
            file,
        })
    }

    async fn rebuild_from_boot(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        path: PathBuf,
        generation: u64,
        journal: &[RawEnvelope],
    ) -> Result<ReconciledSidecar, PipeNativeError> {
        let (mut file, temp_path) = create_temp(path.clone()).await?;
        let generation = generation
            .checked_add(1)
            .ok_or_else(|| PipeNativeError("sidecar generation exhausted".into()))?;
        file = write_temp(file, header_line(session_id, generation, 0, 0)?).await?;
        let mut segment = 0;
        let mut sealed_root = None;
        let mut projector = TranscriptProjector::default();
        (file, segment) = write_segmented_open(
            file,
            render_rows_after(journal, 0, &mut projector),
            &path,
            session_id,
            generation,
            segment,
            true,
            &mut sealed_root,
        )
        .await?;
        let mut read_cursor = journal.last().map_or(0, |envelope| envelope.seq);
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
            (file, segment) = write_segmented_open(
                file,
                render_rows_after(&page, 0, &mut projector),
                &path,
                session_id,
                generation,
                segment,
                true,
                &mut sealed_root,
            )
            .await?;
        }
        (file, segment) = write_segmented_open(
            file,
            render_projected_rows(projector.flush_unresolved_tools()),
            &path,
            session_id,
            generation,
            segment,
            true,
            &mut sealed_root,
        )
        .await?;
        let covered = projector
            .blocked_seq()
            .map_or(read_cursor, |seq| seq.saturating_sub(1));
        file = write_open(file, coverage_line(covered, generation)?).await?;
        let file = if segment == 0 {
            finish_temp(file, temp_path, path.clone()).await?;
            open_append(path.clone()).await?
        } else {
            let file = sync_open(file).await?;
            let root_file = sealed_root.take().ok_or_else(|| {
                PipeNativeError("segmented rebuild lost its sealed root handle".into())
            })?;
            finish_temp(root_file, temp_path, path.clone()).await?;
            file
        };
        Ok(ReconciledSidecar {
            cursor: SidecarCursor {
                seq: covered,
                pending_seq: read_cursor,
                generation,
                segment,
            },
            projector,
            base_path: path,
            file,
        })
    }

    async fn maintain_inner(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        committed: &[RawEnvelope],
    ) -> Result<(), PipeNativeError> {
        let path = self.sidecar_path(session_id)?;
        let known = self
            .reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        let first_touch = known.is_none();

        let dirty = self
            .dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id);
        let state = if let Some(mut state) = known {
            let durable_head = store.latest_seq(session_id).await.map_err(|error| {
                PipeNativeError(format!("journal head inspection failed: {error:?}"))
            })?;
            let (data, mut next_cursor) =
                render_hot_batch(committed, durable_head, state.cursor, &mut state.projector)?;
            if !data.is_empty() {
                let mut sealed_root = None;
                let (file, segment) = write_segmented_open(
                    state.file,
                    data,
                    &state.base_path,
                    session_id,
                    state.cursor.generation,
                    state.cursor.segment,
                    false,
                    &mut sealed_root,
                )
                .await?;
                state.file = file;
                next_cursor.segment = segment;
            }
            state.cursor = next_cursor;
            state
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

        if first_touch {
            sweep_orphan_segments_best_effort(&state.base_path, session_id).await;
        }

        self.reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.clone(), state);
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
    ) -> Result<ReconciledSidecar, PipeNativeError> {
        let mut projector = prewarm_projector(store, session_id, cursor.seq).await?;
        let mut read_cursor = cursor.seq;
        let active_path = segment_path(&path, cursor.generation, cursor.segment)?;
        let mut file = open_append(active_path).await?;
        let mut segment = cursor.segment;
        let mut sealed_root = None;
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
            let chunk = render_rows_after(&page, cursor.seq, &mut projector);
            if !chunk.is_empty() {
                (file, segment) = write_segmented_open(
                    file,
                    chunk,
                    &path,
                    session_id,
                    cursor.generation,
                    segment,
                    false,
                    &mut sealed_root,
                )
                .await?;
            }
        }
        let trailing = render_projected_rows(projector.flush_unresolved_tools());
        if !trailing.is_empty() {
            (file, segment) = write_segmented_open(
                file,
                trailing,
                &path,
                session_id,
                cursor.generation,
                segment,
                false,
                &mut sealed_root,
            )
            .await?;
        }
        let covered = projector
            .blocked_seq()
            .map_or(read_cursor, |seq| seq.saturating_sub(1));
        let file = write_open(file, coverage_line(covered, cursor.generation)?).await?;
        // Reconciliation is a repair path, so retain an explicit sync here.
        // The synced handle is kept for subsequent hot appends.
        let file = sync_open(file).await?;
        Ok(ReconciledSidecar {
            cursor: SidecarCursor {
                seq: covered,
                pending_seq: read_cursor,
                segment,
                ..cursor
            },
            projector,
            base_path: path,
            file,
        })
    }

    async fn rebuild(
        &self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        path: PathBuf,
        generation: u64,
    ) -> Result<ReconciledSidecar, PipeNativeError> {
        let (mut file, temp_path) = create_temp(path.clone()).await?;
        let generation = generation
            .checked_add(1)
            .ok_or_else(|| PipeNativeError("sidecar generation exhausted".into()))?;
        file = write_temp(file, header_line(session_id, generation, 0, 0)?).await?;
        let mut segment = 0;
        let mut sealed_root = None;
        let mut read_cursor = 0;
        let mut projector = TranscriptProjector::default();
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
            let chunk = render_rows_after(&page, 0, &mut projector);
            (file, segment) = write_segmented_open(
                file,
                chunk,
                &path,
                session_id,
                generation,
                segment,
                true,
                &mut sealed_root,
            )
            .await?;
        }
        (file, segment) = write_segmented_open(
            file,
            render_projected_rows(projector.flush_unresolved_tools()),
            &path,
            session_id,
            generation,
            segment,
            true,
            &mut sealed_root,
        )
        .await?;
        let covered = projector
            .blocked_seq()
            .map_or(read_cursor, |seq| seq.saturating_sub(1));
        file = write_open(file, coverage_line(covered, generation)?).await?;
        let file = if segment == 0 {
            finish_temp(file, temp_path, path.clone()).await?;
            open_append(path.clone()).await?
        } else {
            let file = sync_open(file).await?;
            let root_file = sealed_root.take().ok_or_else(|| {
                PipeNativeError("segmented rebuild lost its sealed root handle".into())
            })?;
            finish_temp(root_file, temp_path, path.clone()).await?;
            file
        };
        Ok(ReconciledSidecar {
            cursor: SidecarCursor {
                seq: covered,
                pending_seq: read_cursor,
                generation,
                segment,
            },
            projector,
            base_path: path,
            file,
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

async fn prewarm_projector(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    through_seq: u64,
) -> Result<TranscriptProjector, PipeNativeError> {
    let mut projector = TranscriptProjector::default();
    let mut read_cursor = through_seq.saturating_sub(JOIN_PREWARM_ENVELOPES);
    while read_cursor < through_seq {
        let page = store
            .read_page(
                session_id,
                read_cursor,
                RECONCILE_PAGE_ENVELOPES,
                RECONCILE_PAGE_BYTES,
            )
            .await
            .map_err(|error| PipeNativeError(format!("journal join prewarm failed: {error:?}")))?;
        let mut advanced = false;
        for envelope in ordered_after(&page, read_cursor) {
            if envelope.seq > through_seq {
                break;
            }
            // These rows are already represented by the durable cursor. Only
            // rebuild call/result join state; normal projection here would
            // buffer and later duplicate an unresolved row from the sidecar.
            projector.prewarm(envelope);
            read_cursor = envelope.seq;
            advanced = true;
        }
        if !advanced {
            break;
        }
    }
    Ok(projector)
}

#[allow(clippy::too_many_arguments)]
async fn append_store_suffix(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    base_path: PathBuf,
    mut file: File,
    mut segment: u64,
    generation: u64,
    projector: &mut TranscriptProjector,
    read_cursor: &mut u64,
) -> Result<(File, u64), PipeNativeError> {
    let mut sealed_root = None;
    loop {
        let page = store
            .read_page(
                session_id,
                *read_cursor,
                RECONCILE_PAGE_ENVELOPES,
                RECONCILE_PAGE_BYTES,
            )
            .await
            .map_err(|error| {
                PipeNativeError(format!("journal reconciliation failed: {error:?}"))
            })?;
        let Some(last) = page.last() else {
            return Ok((file, segment));
        };
        *read_cursor = last.seq;
        let chunk = render_rows_after(&page, 0, projector);
        if !chunk.is_empty() {
            (file, segment) = write_segmented_open(
                file,
                chunk,
                &base_path,
                session_id,
                generation,
                segment,
                false,
                &mut sealed_root,
            )
            .await?;
        }
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

fn render_rows_after(
    envelopes: &[RawEnvelope],
    cursor: u64,
    projector: &mut TranscriptProjector,
) -> String {
    let mut data = String::new();
    for envelope in ordered_after(envelopes, cursor) {
        data.push_str(&render_projected_rows(projector.push(envelope)));
    }
    data
}

fn render_projected_rows(
    rows: impl IntoIterator<Item = haider_protocol::pipe::SidecarRow>,
) -> String {
    let mut data = String::new();
    for row in rows {
        if let Ok(line) = serde_json::to_string(&row) {
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

fn header_line(
    session_id: &SessionId,
    generation: u64,
    segment: u64,
    starts_after: u64,
) -> Result<String, PipeNativeError> {
    let mut line = serde_json::to_string(&SidecarHeader {
        pipe: SIDECAR_MAGIC.to_owned(),
        version: SIDECAR_VERSION,
        session_id: session_id.as_str().to_owned(),
        generation,
        segment,
        starts_after,
    })
    .map_err(|error| PipeNativeError(format!("sidecar header serialization failed: {error}")))?;
    line.push('\n');
    Ok(line)
}

fn segment_path(
    base_path: &Path,
    generation: u64,
    segment: u64,
) -> Result<PathBuf, PipeNativeError> {
    if segment == 0 {
        return Ok(base_path.to_owned());
    }
    let parent = base_path
        .parent()
        .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
    let stem = base_path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| PipeNativeError("sidecar path has no UTF-8 file stem".into()))?;
    Ok(parent.join(format!("{stem}.g{generation}.s{segment}.pipe")))
}

async fn sweep_orphan_segments_best_effort(base_path: &Path, session_id: &SessionId) {
    let base_path = base_path.to_owned();
    let session_for_sweep = session_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        sweep_orphan_segments_blocking(&base_path, &session_for_sweep)
    })
    .await;
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!(
            session_id = %session_id,
            error = %error,
            "native pipe orphan sweep skipped; live chain was left untouched"
        ),
        Err(error) => tracing::warn!(
            session_id = %session_id,
            error = %error,
            "native pipe orphan sweep task failed; live chain was left untouched"
        ),
    }
}

/// Deletes only same-session segment files proven unreachable from the live
/// root. Reachability is a property of the on-disk successor pointers, never
/// of generation/segment-looking filename adjacency. The full pointer walk
/// and candidate discovery both complete before the first unlink.
fn sweep_orphan_segments_blocking(
    base_path: &Path,
    session_id: &SessionId,
) -> Result<usize, PipeNativeError> {
    let reachable = reachable_sidecar_paths(base_path, session_id)?;
    let candidates = orphan_segment_candidates(base_path, session_id, &reachable)?;
    if candidates.is_empty() {
        return Ok(0);
    }
    let mut swept = 0;
    for candidate in &candidates {
        if quarantine_and_sweep_candidate(base_path, session_id, candidate)? {
            swept += 1;
        }
    }
    let parent = base_path
        .parent()
        .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
    haider_platform::sync_directory(parent)?;
    Ok(swept)
}

/// Atomically detaches a candidate pathname before trusting its contents,
/// then repeats both ownership validation and the live-root pointer walk.
/// The profile singleton and per-session writer serialize cooperating chain
/// mutations; quarantine additionally prevents a pathname replacement from
/// making us unlink bytes other than the bytes inspected here.
fn quarantine_and_sweep_candidate(
    base_path: &Path,
    session_id: &SessionId,
    candidate: &Path,
) -> Result<bool, PipeNativeError> {
    let Some(quarantine) = quarantine_candidate(base_path, candidate)? else {
        return Ok(false);
    };
    let header = match read_sidecar_header_without_following(&quarantine) {
        Ok(header) => header,
        Err(error) => {
            restore_quarantined_candidate(&quarantine, candidate).map_err(|restore_error| {
                PipeNativeError(format!(
                    "{error}; quarantined file was preserved but could not be restored: {restore_error}"
                ))
            })?;
            return Err(error);
        }
    };
    let owns_segment = match header_owns_segment(base_path, session_id, candidate, &header) {
        Ok(owns_segment) => owns_segment,
        Err(error) => {
            restore_quarantined_candidate(&quarantine, candidate).map_err(|restore_error| {
                PipeNativeError(format!(
                    "{error}; quarantined file was preserved but could not be restored: {restore_error}"
                ))
            })?;
            return Err(error);
        }
    };
    if !owns_segment {
        let error = PipeNativeError(format!(
            "quarantined sidecar candidate changed ownership: {}",
            candidate.display()
        ));
        restore_quarantined_candidate(&quarantine, candidate).map_err(|restore_error| {
            PipeNativeError(format!(
                "{error}; quarantined file was preserved but could not be restored: {restore_error}"
            ))
        })?;
        return Err(error);
    }
    let reachable = match reachable_sidecar_paths(base_path, session_id) {
        Ok(reachable) => reachable,
        Err(error) => {
            restore_quarantined_candidate(&quarantine, candidate).map_err(|restore_error| {
                PipeNativeError(format!(
                    "{error}; quarantined file was preserved but could not be restored: {restore_error}"
                ))
            })?;
            return Err(error);
        }
    };
    if reachable.contains(&quarantine) {
        // The live chain now names the quarantine path. Moving or deleting it
        // would destroy history, so preserve it exactly where the pointer found it.
        return Err(PipeNativeError(format!(
            "quarantined sidecar candidate became reachable: {}",
            quarantine.display()
        )));
    }
    if reachable.contains(candidate) {
        let error = PipeNativeError(format!(
            "sidecar candidate pathname became reachable while quarantined: {}",
            candidate.display()
        ));
        restore_quarantined_candidate(&quarantine, candidate).map_err(|restore_error| {
            PipeNativeError(format!(
                "{error}; quarantined file was preserved but could not be restored: {restore_error}"
            ))
        })?;
        return Err(error);
    }
    std::fs::remove_file(&quarantine)?;
    Ok(true)
}

fn quarantine_candidate(
    base_path: &Path,
    candidate: &Path,
) -> Result<Option<PathBuf>, PipeNativeError> {
    let parent = base_path
        .parent()
        .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
    let stem = base_path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| PipeNativeError("sidecar path has no UTF-8 file stem".into()))?;
    loop {
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let quarantine = parent.join(format!(
            ".{stem}.orphan-sweep-{}-{suffix}.tmp",
            std::process::id()
        ));
        match rename_noreplace(candidate, &quarantine) {
            Ok(()) => return Ok(Some(quarantine)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
}

fn restore_quarantined_candidate(
    quarantine: &Path,
    candidate: &Path,
) -> Result<(), PipeNativeError> {
    rename_noreplace(quarantine, candidate).map_err(Into::into)
}

#[cfg(unix)]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "sidecar quarantine destination already exists",
        ));
    }
    std::fs::rename(from, to)
}

fn reachable_sidecar_paths(
    base_path: &Path,
    session_id: &SessionId,
) -> Result<HashSet<PathBuf>, PipeNativeError> {
    let parent = base_path
        .parent()
        .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
    let mut reachable = HashSet::new();
    let mut current_path = base_path.to_owned();
    let mut expected_segment = 0_u64;
    let mut expected_starts_after = 0_u64;
    let mut chain_generation = None;
    loop {
        if !reachable.insert(current_path.clone()) {
            return Err(PipeNativeError(
                "sidecar successor chain contains a loop".into(),
            ));
        }
        let data = read_sidecar_without_following(&current_path)?;
        if data.is_empty() || !data.ends_with(b"\n") {
            return Err(PipeNativeError(format!(
                "sidecar chain member is empty or torn: {}",
                current_path.display()
            )));
        }
        let text = std::str::from_utf8(&data).map_err(|_| {
            PipeNativeError(format!(
                "sidecar chain member is not UTF-8: {}",
                current_path.display()
            ))
        })?;
        let mut lines = text.strip_suffix('\n').unwrap_or(text).split('\n');
        let header_line = lines.next().ok_or_else(|| {
            PipeNativeError(format!(
                "sidecar chain member has no header: {}",
                current_path.display()
            ))
        })?;
        let header: SidecarHeader = serde_json::from_str(header_line).map_err(|error| {
            PipeNativeError(format!(
                "sidecar chain header is unreadable at {}: {error}",
                current_path.display()
            ))
        })?;
        let generation = chain_generation.unwrap_or(header.generation);
        if header.pipe != SIDECAR_MAGIC
            || header.version != SIDECAR_VERSION
            || header.session_id != session_id.as_str()
            || header.generation == 0
            || header.generation != generation
            || header.segment != expected_segment
            || header.starts_after != expected_starts_after
        {
            return Err(PipeNativeError(format!(
                "sidecar chain header does not match the live root at {}",
                current_path.display()
            )));
        }
        chain_generation = Some(generation);

        let body = lines
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).map_err(|error| {
                    PipeNativeError(format!(
                        "sidecar chain row is unreadable at {}: {error}",
                        current_path.display()
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let Some(tail) = body.last() else {
            return Ok(reachable);
        };
        if body[..body.len() - 1]
            .iter()
            .any(|value| value.get("segment_end").is_some())
        {
            return Err(PipeNativeError(format!(
                "sidecar chain has a non-final terminator at {}",
                current_path.display()
            )));
        }
        if tail.get("segment_end").is_some() {
            let end: SidecarSegmentEnd = serde_json::from_value(tail.clone()).map_err(|error| {
                PipeNativeError(format!(
                    "sidecar chain terminator is unreadable at {}: {error}",
                    current_path.display()
                ))
            })?;
            let previous_is_boundary =
                body.get(body.len().saturating_sub(2))
                    .is_some_and(|previous| {
                        previous.get("kind").and_then(serde_json::Value::as_str)
                            == Some("compaction_boundary")
                            && previous.get("seq").and_then(serde_json::Value::as_u64)
                                == Some(end.coverage)
                    });
            if end.segment_end != "sealed"
                || end.generation != generation
                || end.coverage < expected_starts_after
                || !previous_is_boundary
            {
                return Err(PipeNativeError(format!(
                    "sidecar chain terminator is inconsistent at {}",
                    current_path.display()
                )));
            }
            current_path = direct_successor_path(parent, &end.successor)?;
            expected_segment = expected_segment
                .checked_add(1)
                .ok_or_else(|| PipeNativeError("sidecar segment ordinal exhausted".into()))?;
            expected_starts_after = end.coverage;
            continue;
        }

        let boundary_shaped = tail.get("kind").and_then(serde_json::Value::as_str)
            == Some("compaction_boundary")
            && tail
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .is_some();
        let row_shaped = tail
            .get("role")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|role| matches!(role, "user" | "assistant" | "error" | "tool"))
            && tail
                .get("at_ms")
                .and_then(serde_json::Value::as_u64)
                .is_some();
        let coverage_shaped = tail
            .get("coverage")
            .and_then(serde_json::Value::as_u64)
            .is_some()
            && tail.get("generation").and_then(serde_json::Value::as_u64) == Some(generation);
        if boundary_shaped || (!row_shaped && !coverage_shaped) {
            return Err(PipeNativeError(format!(
                "sidecar chain has an invalid active tail at {}",
                current_path.display()
            )));
        }
        let seq = tail
            .get(if row_shaped { "seq" } else { "coverage" })
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                PipeNativeError(format!(
                    "sidecar chain tail has no sequence at {}",
                    current_path.display()
                ))
            })?;
        if seq < expected_starts_after {
            return Err(PipeNativeError(format!(
                "sidecar chain tail regresses coverage at {}",
                current_path.display()
            )));
        }
        return Ok(reachable);
    }
}

fn direct_successor_path(parent: &Path, successor: &str) -> Result<PathBuf, PipeNativeError> {
    let mut components = Path::new(successor).components();
    let Some(std::path::Component::Normal(filename)) = components.next() else {
        return Err(PipeNativeError(
            "sidecar successor is not one direct-child filename".into(),
        ));
    };
    if components.next().is_some() {
        return Err(PipeNativeError(
            "sidecar successor is not one direct-child filename".into(),
        ));
    }
    Ok(parent.join(filename))
}

fn orphan_segment_candidates(
    base_path: &Path,
    session_id: &SessionId,
    reachable: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>, PipeNativeError> {
    let parent = base_path
        .parent()
        .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
    let stem = base_path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| PipeNativeError("sidecar path has no UTF-8 file stem".into()))?;
    let prefix = format!("{stem}.g");
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        if !filename.starts_with(&prefix) || !filename.ends_with(".pipe") {
            continue;
        }
        let path = entry.path();
        if reachable.contains(&path) || entry.file_type()?.is_symlink() {
            continue;
        }
        let Ok(header) = read_sidecar_header_without_following(&path) else {
            // An unreadable candidate is not proven to be ours.
            continue;
        };
        if header_owns_segment(base_path, session_id, &path, &header)? {
            candidates.push(path);
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn header_owns_segment(
    base_path: &Path,
    session_id: &SessionId,
    candidate_path: &Path,
    header: &SidecarHeader,
) -> Result<bool, PipeNativeError> {
    if header.pipe != SIDECAR_MAGIC
        || header.session_id != session_id.as_str()
        || header.generation == 0
        || header.segment == 0
    {
        return Ok(false);
    }
    // The header, not parsed filename components, proves both ownership and
    // the canonical segment filename. It conveys no reachability; that comes
    // exclusively from the live-root pointer walk.
    Ok(segment_path(base_path, header.generation, header.segment)? == candidate_path)
}

fn read_sidecar_header_without_following(path: &Path) -> Result<SidecarHeader, PipeNativeError> {
    let mut file = open_sidecar_readonly(path)?;
    let mut header = String::new();
    BufReader::new(&mut file).read_line(&mut header)?;
    serde_json::from_str(header.trim_end_matches('\n')).map_err(|error| {
        PipeNativeError(format!(
            "sidecar candidate header is unreadable at {}: {error}",
            path.display()
        ))
    })
}

fn read_sidecar_without_following(path: &Path) -> Result<Vec<u8>, PipeNativeError> {
    let mut file = open_sidecar_readonly(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}

#[cfg(unix)]
fn open_sidecar_readonly(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn open_sidecar_readonly(path: &Path) -> std::io::Result<File> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sidecar path is a symlink",
        ));
    }
    OpenOptions::new().read(true).open(path)
}

fn segment_end_line(
    coverage: u64,
    generation: u64,
    successor: &Path,
) -> Result<String, PipeNativeError> {
    let successor = successor
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| PipeNativeError("sidecar successor has no UTF-8 filename".into()))?;
    let mut line = serde_json::to_string(&SidecarSegmentEnd {
        segment_end: "sealed".into(),
        coverage,
        generation,
        successor: successor.to_owned(),
    })
    .map_err(|error| {
        PipeNativeError(format!(
            "sidecar segment terminator serialization failed: {error}"
        ))
    })?;
    line.push('\n');
    Ok(line)
}

fn compaction_boundary_seq(line: &str) -> Option<u64> {
    let value = serde_json::from_str::<serde_json::Value>(line.trim_end_matches('\n')).ok()?;
    (value.get("kind").and_then(serde_json::Value::as_str) == Some("compaction_boundary"))
        .then(|| value.get("seq").and_then(serde_json::Value::as_u64))?
}

async fn create_successor_segment(
    base_path: &Path,
    session_id: &SessionId,
    generation: u64,
    segment: u64,
    starts_after: u64,
) -> Result<(File, PathBuf), PipeNativeError> {
    let path = segment_path(base_path, generation, segment)?;
    let (mut file, temp_path) = create_temp(path.clone()).await?;
    file = write_temp(
        file,
        header_line(session_id, generation, segment, starts_after)?,
    )
    .await?;
    finish_temp(file, temp_path, path.clone()).await?;
    Ok((open_append(path.clone()).await?, path))
}

/// Writes projected rows and rotates only after a compacting turn's terminal
/// boundary row. `hold_root` keeps a rebuild's segment-zero temp handle aside
/// so the stable root is renamed last, after every successor exists.
#[allow(clippy::too_many_arguments)]
async fn write_segmented_open(
    mut file: File,
    data: String,
    base_path: &Path,
    session_id: &SessionId,
    generation: u64,
    mut segment: u64,
    hold_root: bool,
    sealed_root: &mut Option<File>,
) -> Result<(File, u64), PipeNativeError> {
    let mut pending = String::new();
    for line in data.split_inclusive('\n') {
        pending.push_str(line);
        let Some(boundary_seq) = compaction_boundary_seq(line) else {
            continue;
        };
        let successor_segment = segment
            .checked_add(1)
            .ok_or_else(|| PipeNativeError("sidecar segment ordinal exhausted".into()))?;
        // Create and durably publish the successor before making it reachable
        // from the sealed predecessor.
        let (successor_file, successor_path) = create_successor_segment(
            base_path,
            session_id,
            generation,
            successor_segment,
            boundary_seq,
        )
        .await?;
        pending.push_str(&segment_end_line(
            boundary_seq,
            generation,
            &successor_path,
        )?);
        file = write_open(file, std::mem::take(&mut pending)).await?;
        file = sync_open(file).await?;
        if hold_root && segment == 0 {
            *sealed_root = Some(file);
        }
        file = successor_file;
        segment = successor_segment;
    }
    if !pending.is_empty() {
        file = write_open(file, pending).await?;
    }
    Ok((file, segment))
}

/// Render an ordinary committed batch. A row-producing batch gets exactly
/// one trailing watermark. Non-projecting batches coalesce until 256 newly
/// covered envelopes can be represented by one watermark.
fn render_hot_batch(
    envelopes: &[RawEnvelope],
    durable_head: u64,
    cursor: SidecarCursor,
    projector: &mut TranscriptProjector,
) -> Result<(String, SidecarCursor), PipeNativeError> {
    let ordered = ordered_after(envelopes, cursor.pending_seq);
    let pending_seq = ordered
        .last()
        .map_or(cursor.pending_seq, |envelope| envelope.seq);
    let mut data = String::new();
    let mut produced_row = false;
    for envelope in ordered {
        let rows = projector.push(envelope);
        if !rows.is_empty() {
            data.push_str(&render_projected_rows(rows));
            produced_row = true;
        }
    }
    // A queued writer batch can lag a later SQLite commit. Preserve the join
    // while it is not at EOF; once this projector has processed the durable
    // head, use the exact same unresolved-tool EOF rule as every cold path
    // and the prompt boundary fold.
    if pending_seq == durable_head {
        let trailing = projector.flush_unresolved_tools();
        produced_row |= !trailing.is_empty();
        data.push_str(&render_projected_rows(trailing));
    }
    let coverable_seq = projector
        .blocked_seq()
        .map_or(pending_seq, |seq| seq.saturating_sub(1));
    let should_cover =
        produced_row || coverable_seq.saturating_sub(cursor.seq) >= COVERAGE_COALESCE_ENVELOPES;
    let seq = if should_cover {
        let seq = coverable_seq.max(cursor.seq);
        data.push_str(&coverage_line(seq, cursor.generation)?);
        seq
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
    let mut current_path = path.to_owned();
    let mut expected_segment = 0_u64;
    let mut expected_starts_after = 0_u64;
    let mut chain_generation = None;
    loop {
        let mut file = match open_sidecar_for_inspection(&current_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && expected_segment == 0 => {
                return Ok(SidecarState::Missing);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SidecarState::Corrupt {
                    generation: chain_generation.unwrap_or(0),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut len = file.metadata()?.len();
        if len == 0 {
            return Ok(SidecarState::Corrupt {
                generation: chain_generation.unwrap_or(0),
            });
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
            return Ok(SidecarState::Corrupt {
                generation: chain_generation.unwrap_or(0),
            });
        }

        file.seek(SeekFrom::Start(0))?;
        let mut header_line = String::new();
        let header_len = BufReader::new(&mut file).read_line(&mut header_line)? as u64;
        let Ok(header) = serde_json::from_str::<SidecarHeader>(header_line.trim_end_matches('\n'))
        else {
            return Ok(SidecarState::Corrupt {
                generation: chain_generation.unwrap_or(0),
            });
        };
        let generation = chain_generation.unwrap_or(header.generation);
        if header.pipe != SIDECAR_MAGIC
            || header.version != SIDECAR_VERSION
            || header.session_id != session_id.as_str()
            || header.generation == 0
            || header.generation != generation
            || header.segment != expected_segment
            || header.starts_after != expected_starts_after
        {
            return Ok(SidecarState::Corrupt {
                generation: header.generation.max(generation),
            });
        }
        chain_generation = Some(generation);
        if len == header_len {
            return Ok(SidecarState::Ready(SidecarCursor {
                seq: header.starts_after,
                pending_seq: header.starts_after,
                generation,
                segment: header.segment,
            }));
        }

        let (line_start, line) = read_tail_line(&mut file, len)?;
        if contains_nonfinal_segment_end(&mut file, header_len, line_start)? {
            return Ok(SidecarState::Corrupt { generation });
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            return Ok(SidecarState::Corrupt { generation });
        };
        if value.get("segment_end").is_some() {
            let Ok(end) = serde_json::from_value::<SidecarSegmentEnd>(value) else {
                return Ok(SidecarState::Corrupt { generation });
            };
            let successor_segment = expected_segment
                .checked_add(1)
                .ok_or_else(|| PipeNativeError("sidecar segment ordinal exhausted".into()))?;
            let successor_path = segment_path(path, generation, successor_segment)?;
            let expected_successor = successor_path.file_name().and_then(std::ffi::OsStr::to_str);
            let previous_is_boundary = read_line_before(&mut file, line_start)?
                .and_then(|(_, line)| serde_json::from_str::<serde_json::Value>(&line).ok())
                .is_some_and(|previous| {
                    previous.get("kind").and_then(serde_json::Value::as_str)
                        == Some("compaction_boundary")
                        && previous.get("seq").and_then(serde_json::Value::as_u64)
                            == Some(end.coverage)
                });
            if end.segment_end != "sealed"
                || end.generation != generation
                || end.successor != expected_successor.unwrap_or_default()
                || end.coverage < expected_starts_after
                || !previous_is_boundary
            {
                return Ok(SidecarState::Corrupt { generation });
            }
            current_path = successor_path;
            expected_segment = successor_segment;
            expected_starts_after = end.coverage;
            continue;
        }

        // A boundary without its sealed terminator is a torn rotation. Rebuild
        // from the journal instead of accepting it as the active EOF.
        let boundary_shaped = value.get("kind").and_then(serde_json::Value::as_str)
            == Some("compaction_boundary")
            && value
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .is_some();
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
            && value.get("generation").and_then(serde_json::Value::as_u64) == Some(generation);
        if boundary_shaped || (!row_shaped && !coverage_shaped) {
            return Ok(SidecarState::Corrupt { generation });
        }
        let Some(seq) = value
            .get(if row_shaped { "seq" } else { "coverage" })
            .and_then(serde_json::Value::as_u64)
        else {
            return Ok(SidecarState::Corrupt { generation });
        };
        if seq < expected_starts_after {
            return Ok(SidecarState::Corrupt { generation });
        }
        return Ok(SidecarState::Ready(SidecarCursor {
            seq,
            pending_seq: seq,
            generation,
            segment: expected_segment,
        }));
    }
}

#[cfg(unix)]
fn open_sidecar_for_inspection(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::open(
        path,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn open_sidecar_for_inspection(path: &Path) -> std::io::Result<File> {
    // Unix uses O_NOFOLLOW above so repair truncation can never traverse a
    // symlink. Other platforms retain the existing open behavior until their
    // equivalent reparse-point-safe handle validation is available.
    OpenOptions::new().read(true).write(true).open(path)
}

fn contains_nonfinal_segment_end(
    file: &mut File,
    header_len: u64,
    final_line_start: u64,
) -> Result<bool, PipeNativeError> {
    if final_line_start <= header_len {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(header_len))?;
    let mut earlier = BufReader::new(file.take(final_line_start - header_len));
    let mut line = String::new();
    while earlier.read_line(&mut line)? != 0 {
        if serde_json::from_str::<serde_json::Value>(line.trim_end_matches('\n'))
            .ok()
            .is_some_and(|value| value.get("segment_end").is_some())
        {
            return Ok(true);
        }
        line.clear();
    }
    Ok(false)
}

fn read_tail_line(file: &mut File, len: u64) -> Result<(u64, String), PipeNativeError> {
    let line_start = find_previous_newline(file, len - 1)?.map_or(0, |position| position + 1);
    let line_len = usize::try_from((len - 1) - line_start)
        .map_err(|_| PipeNativeError("sidecar tail is too large to inspect".into()))?;
    let mut bytes = vec![0_u8; line_len];
    file.seek(SeekFrom::Start(line_start))?;
    file.read_exact(&mut bytes)?;
    let line = String::from_utf8(bytes)
        .map_err(|_| PipeNativeError("sidecar tail is not UTF-8".into()))?;
    Ok((line_start, line))
}

fn read_line_before(
    file: &mut File,
    line_start: u64,
) -> Result<Option<(u64, String)>, PipeNativeError> {
    if line_start == 0 {
        return Ok(None);
    }
    let newline = line_start - 1;
    let previous_start = find_previous_newline(file, newline)?.map_or(0, |position| position + 1);
    let line_len = usize::try_from(newline - previous_start)
        .map_err(|_| PipeNativeError("sidecar prior tail line is too large".into()))?;
    let mut bytes = vec![0_u8; line_len];
    file.seek(SeekFrom::Start(previous_start))?;
    file.read_exact(&mut bytes)?;
    let line = String::from_utf8(bytes)
        .map_err(|_| PipeNativeError("sidecar prior tail line is not UTF-8".into()))?;
    Ok(Some((previous_start, line)))
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

async fn sync_open(file: File) -> Result<File, PipeNativeError> {
    tokio::task::spawn_blocking(move || {
        file.sync_data()?;
        Ok(file)
    })
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
        // Windows replacement requires the staged file handle to be closed.
        drop(file);
        haider_platform::replace_file(&temp_path, &path).inspect_err(|_error| {
            let _ = std::fs::remove_file(&temp_path);
        })?;
        let parent = path
            .parent()
            .ok_or_else(|| PipeNativeError("sidecar path has no parent".into()))?;
        haider_platform::sync_directory(parent)?;
        Ok(())
    })
    .await
    .map_err(|error| PipeNativeError(format!("sidecar rebuild finalize task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    /// MUTATION CHECK: remove the one-line boot-path
    /// `sweep_orphan_segments_best_effort` call. Expected RUNTIME failure:
    /// the old-generation orphan remains after boot reconciliation succeeds.
    #[tokio::test]
    async fn boot_first_touch_sweeps_previous_generation_orphan() {
        let root = tempfile::tempdir().expect("temp directory");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        let writer = PipeNativeWriter::new(root.path());
        let session_id = SessionId::new("pipe-boot-orphan-sweep");
        let base = writer.sidecar_path(&session_id).expect("sidecar path");
        std::fs::create_dir_all(base.parent().expect("pipe directory"))
            .expect("pipe directory creates");
        std::fs::write(
            &base,
            format!(
                "{}{}",
                header_line(&session_id, 7, 0, 0).expect("root header serializes"),
                coverage_line(0, 7).expect("root coverage serializes")
            ),
        )
        .expect("root fixture writes");
        let orphan = segment_path(&base, 6, 1).expect("orphan path");
        std::fs::write(
            &orphan,
            format!(
                "{}{}",
                header_line(&session_id, 6, 1, 0).expect("orphan header serializes"),
                coverage_line(0, 6).expect("orphan coverage serializes")
            ),
        )
        .expect("orphan fixture writes");

        writer
            .maintain_from_boot_journal(&store, &session_id, &[])
            .await
            .expect("boot reconciliation succeeds");

        assert!(base.exists(), "live root survives boot sweep");
        assert!(!orphan.exists(), "old-generation orphan is swept");
        drop(writer);
        store.close().await.expect("store closes");
    }

    /// MUTATION CHECK: delete the one-line post-quarantine ownership
    /// revalidation. Expected RUNTIME failure: a foreign file replacing the
    /// discovered candidate is unlinked instead of restored intact.
    #[test]
    fn orphan_sweep_revalidates_the_exact_quarantined_file() {
        let root = tempfile::tempdir().expect("temp directory");
        let session_id = SessionId::new("pipe-quarantine-owner");
        let foreign_session = SessionId::new("pipe-quarantine-foreign");
        let base = root.path().join("pipe-quarantine-owner.pipe");
        std::fs::write(
            &base,
            format!(
                "{}{}",
                header_line(&session_id, 7, 0, 0).expect("root header serializes"),
                coverage_line(0, 7).expect("root coverage serializes")
            ),
        )
        .expect("root fixture writes");
        let candidate = segment_path(&base, 6, 1).expect("candidate path");
        std::fs::write(
            &candidate,
            format!(
                "{}{}",
                header_line(&session_id, 6, 1, 0).expect("candidate header serializes"),
                coverage_line(0, 6).expect("candidate coverage serializes")
            ),
        )
        .expect("candidate fixture writes");
        let reachable = reachable_sidecar_paths(&base, &session_id).expect("live chain validates");
        let candidates =
            orphan_segment_candidates(&base, &session_id, &reachable).expect("candidates list");
        assert_eq!(candidates, vec![candidate.clone()]);

        let foreign_contents = format!(
            "{}{}",
            header_line(&foreign_session, 6, 1, 0).expect("foreign header serializes"),
            coverage_line(0, 6).expect("foreign coverage serializes")
        );
        std::fs::write(&candidate, &foreign_contents).expect("candidate replacement writes");

        assert!(
            quarantine_and_sweep_candidate(&base, &session_id, &candidate).is_err(),
            "ownership change aborts the sweep"
        );
        assert_eq!(
            std::fs::read_to_string(&candidate).expect("replacement survives"),
            foreign_contents
        );
    }

    /// MUTATION CHECK: return the partial reachable set when the one-line
    /// successor read reports a torn file. Expected RUNTIME failure: the
    /// orphan sentinel is deleted even though the live chain is uncertain.
    #[test]
    fn orphan_sweep_with_torn_reachable_chain_deletes_nothing() {
        let root = tempfile::tempdir().expect("temp directory");
        let session_id = SessionId::new("pipe-torn-sweep");
        let base = root.path().join("pipe-torn-sweep.pipe");
        let reachable_successor = segment_path(&base, 7, 1).expect("successor path");
        let mut root_segment = header_line(&session_id, 7, 0, 0).expect("root header serializes");
        root_segment.push_str("{\"kind\":\"compaction_boundary\",\"seq\":4}\n");
        root_segment.push_str(
            &segment_end_line(4, 7, &reachable_successor).expect("terminator serializes"),
        );
        std::fs::write(&base, root_segment).expect("root fixture writes");
        std::fs::write(
            &reachable_successor,
            header_line(&session_id, 7, 1, 4).expect("successor header serializes"),
        )
        .expect("successor fixture writes");
        OpenOptions::new()
            .append(true)
            .open(&reachable_successor)
            .expect("successor opens")
            .write_all(b"{\"coverage\":4,\"generation\":7}")
            .expect("torn tail writes");
        let orphan = segment_path(&base, 6, 1).expect("orphan path");
        std::fs::write(
            &orphan,
            format!(
                "{}{}",
                header_line(&session_id, 6, 1, 0).expect("orphan header serializes"),
                coverage_line(0, 6).expect("orphan coverage serializes")
            ),
        )
        .expect("orphan fixture writes");

        assert!(
            sweep_orphan_segments_blocking(&base, &session_id).is_err(),
            "a torn reachable successor makes the sweep uncertain"
        );
        assert!(
            orphan.exists(),
            "uncertain reachability must leave every orphan candidate untouched"
        );
        assert!(base.exists(), "the live root remains untouched");
        assert!(
            reachable_successor.exists(),
            "the torn reachable segment remains untouched"
        );
    }
}
