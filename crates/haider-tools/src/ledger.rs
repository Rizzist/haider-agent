//! Per-turn filesystem write attribution — the change ledger.
//!
//! Owned invariants:
//! - Entries are keyed by `(session, turn)` and record only *applied*
//!   `FsWrite` effects: callers record after the write is real on disk, never
//!   for denied, conflicted, or failed attempts.
//! - This ledger is the verify gate's future evidence — the W4 verify
//!   predicate consumes it to decide whether a turn changed the workspace —
//!   so an applied write must never be missing here.
//! - `paths` and `summaries` deduplicate in first-touch order for cheap
//!   queries; `writes` keeps every record, preserving per-effect evidence.
//! - [`ChangeLedgerSink`] is synchronous and shareable so the filesystem
//!   worker can append evidence in the same cancellation-shielded critical
//!   section as the rename.

use crate::{ToolError, ToolResult};
use haider_protocol::ids::{EffectId, RunId, SessionId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWriteRecord {
    pub effect: EffectId,
    pub paths: Vec<PathBuf>,
    pub summary: String,
    /// BLAKE3 address of the exact bytes atomically installed on disk.
    pub bytes_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnChanges {
    pub paths: Vec<PathBuf>,
    pub summaries: Vec<String>,
    pub writes: Vec<FsWriteRecord>,
}

impl TurnChanges {
    pub fn has_fs_writes(&self) -> bool {
        !self.writes.is_empty()
    }
}

/// Synchronous port for the post-apply evidence append.
///
/// Implementations must return only after the record is retained or a typed
/// failure is known; this method runs immediately after rename with no
/// cancellation-aware await between the two operations. Clones must address
/// the same logical sink because the blocking worker owns a clone.
pub trait ChangeLedgerSink: Clone + Send + Sync + 'static {
    fn record_fs_write(
        &self,
        session: SessionId,
        turn: RunId,
        record: FsWriteRecord,
    ) -> ToolResult<()>;
}

#[derive(Debug, Default)]
struct LedgerState {
    turns: HashMap<(SessionId, RunId), TurnChanges>,
}

#[derive(Debug, Clone, Default)]
pub struct ChangeLedger {
    state: Arc<Mutex<LedgerState>>,
}

impl ChangeLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_fs_write(
        &self,
        session: SessionId,
        turn: RunId,
        record: FsWriteRecord,
    ) -> ToolResult<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ToolError::ledger("change ledger lock is poisoned"))?;
        let changes = state.turns.entry((session, turn)).or_default();
        for path in &record.paths {
            if !changes.paths.iter().any(|existing| existing == path) {
                changes.paths.push(path.clone());
            }
        }
        if !changes
            .summaries
            .iter()
            .any(|summary| summary == &record.summary)
        {
            changes.summaries.push(record.summary.clone());
        }
        changes.writes.push(record);
        Ok(())
    }

    pub fn changes_for(&self, session: &SessionId, turn: &RunId) -> Option<TurnChanges> {
        self.read_state()
            .turns
            .get(&(session.clone(), turn.clone()))
            .cloned()
    }

    pub fn has_fs_writes(&self, session: &SessionId, turn: &RunId) -> bool {
        self.changes_for(session, turn)
            .is_some_and(|changes| changes.has_fs_writes())
    }

    pub fn path_touched(&self, session: &SessionId, turn: &RunId, path: &Path) -> bool {
        self.changes_for(session, turn)
            .is_some_and(|changes| changes.paths.iter().any(|touched| touched == path))
    }

    fn read_state(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ChangeLedgerSink for ChangeLedger {
    fn record_fs_write(
        &self,
        session: SessionId,
        turn: RunId,
        record: FsWriteRecord,
    ) -> ToolResult<()> {
        Self::record_fs_write(self, session, turn, record)
    }
}
