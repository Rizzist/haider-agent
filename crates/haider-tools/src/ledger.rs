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

use haider_protocol::ids::{EffectId, RunId, SessionId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Default)]
pub struct ChangeLedger {
    turns: HashMap<(SessionId, RunId), TurnChanges>,
}

impl ChangeLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_fs_write(&mut self, session: SessionId, turn: RunId, record: FsWriteRecord) {
        let changes = self.turns.entry((session, turn)).or_default();
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
    }

    pub fn changes_for(&self, session: &SessionId, turn: &RunId) -> Option<&TurnChanges> {
        self.turns.get(&(session.clone(), turn.clone()))
    }

    pub fn has_fs_writes(&self, session: &SessionId, turn: &RunId) -> bool {
        self.changes_for(session, turn)
            .is_some_and(TurnChanges::has_fs_writes)
    }

    pub fn path_touched(&self, session: &SessionId, turn: &RunId, path: &Path) -> bool {
        self.changes_for(session, turn)
            .is_some_and(|changes| changes.paths.iter().any(|touched| touched == path))
    }
}
