//! The profile's TERMS-ACKNOWLEDGEMENT journal — a durable record of the
//! warnings this user was shown and chose to continue past.
//!
//! This is a user DECISION, not a display preference, so it deliberately does
//! not live in `tui-settings.json`: that file documents itself as display
//! state, is rewritten wholesale on every theme save, and has no room for a
//! decision's timestamp. The journal is append-only instead — one JSON object
//! per line beside the settings file in the profile dir, written the same way
//! (`$HAIDER_PROFILE_DIR`, falling back to `~/.haider/dev-profile/`; no home
//! at all simply disables persistence).
//!
//! A record carries only the subject, the wall-clock instant and the exact
//! warning text that was displayed. Nothing else may ever go in: no URL, no
//! query string, no authorization code, no token, no identity. Every field is
//! built from a compile-time constant plus a clock, so that is true by
//! construction rather than by review.
//!
//! Reads are tolerant by design — a truncated tail or a record from a future
//! version is skipped, never a panic and never a clobber, because losing the
//! ability to READ an acknowledgement must not turn into losing the warning.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The journal's file name, beside `tui-settings.json` in the profile dir.
pub const TERMS_JOURNAL_FILE: &str = "terms-acknowledgements.jsonl";

/// Record shape version. A line with any other version is ignored on read and
/// left untouched on write (append-only: nothing is ever rewritten).
const RECORD_VERSION: u32 = 1;

/// One acknowledgement, exactly as it lands on disk.
#[derive(serde::Serialize, serde::Deserialize)]
struct AcknowledgementDto {
    version: u32,
    /// What was acknowledged (e.g. `google-antigravity-terms`).
    subject: String,
    /// Wall clock at the moment the user proceeded, epoch milliseconds.
    acknowledged_at_ms: u64,
    /// The warning text as displayed — the evidence of WHAT was shown.
    warning: String,
}

/// The append-only store.
#[derive(Debug)]
pub struct TermsJournal {
    path: PathBuf,
}

impl TermsJournal {
    /// A journal at an explicit path (tests point this into a temp dir).
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// `$HAIDER_PROFILE_DIR/terms-acknowledgements.jsonl`, falling back to
    /// `~/.haider/dev-profile/` — the resolution the settings and demo stores
    /// already use. `None` (no profile dir and no home) disables persistence.
    #[must_use]
    pub fn open_default() -> Option<Self> {
        crate::settings::SettingsStore::default_path()
            .and_then(|settings| settings.parent().map(Path::to_path_buf))
            .map(|dir| Self::at(dir.join(TERMS_JOURNAL_FILE)))
    }

    /// The resolved file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every subject this profile has already acknowledged. A missing file is
    /// an empty set (nothing acknowledged yet); an unreadable, truncated or
    /// future-version line is skipped rather than failing the read.
    #[must_use]
    pub fn subjects(&self) -> BTreeSet<String> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return BTreeSet::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str::<AcknowledgementDto>(line).ok())
            .filter(|record| record.version == RECORD_VERSION)
            .map(|record| record.subject)
            .collect()
    }

    /// Appends one acknowledgement, unless this subject already carries one.
    /// Returns whether a record was written — so a re-affirmation of the same
    /// decision leaves exactly ONE durable entry, whatever the caller does.
    ///
    /// A write failure (read-only profile, full disk) is not fatal: the
    /// decision still stands for this run, and the warning simply shows again
    /// next time — the safe direction.
    pub fn record(&self, subject: &str, warning: &str, at_ms: u64) -> bool {
        if self.subjects().contains(subject) {
            return false;
        }
        let record = AcknowledgementDto {
            version: RECORD_VERSION,
            subject: subject.to_owned(),
            acknowledged_at_ms: at_ms,
            warning: warning.to_owned(),
        };
        let Ok(mut line) = serde_json::to_string(&record) else {
            return false;
        };
        line.push('\n');
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .is_ok()
    }
}
