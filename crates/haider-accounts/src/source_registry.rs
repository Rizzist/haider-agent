//! Durable registry of externally owned credential roots.
//!
//! The registry contains coordinates and public scan state only. Token bytes
//! remain in the origin store and are re-read at resolution time, so Haider
//! never becomes a second owner of a rotating refresh credential.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use haider_protocol::error::ErrorCode;
use serde::{Deserialize, Serialize};

use crate::{AccountsResult, accounts_error};

pub const CREDENTIAL_SOURCES_FILE_NAME: &str = "credential_sources.json";
const MAX_SOURCES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSourceKind {
    CodexHome,
    ClaudeFile,
}

impl CredentialSourceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CodexHome => "codex_home",
            Self::ClaudeFile => "claude_file",
        }
    }

    #[must_use]
    pub const fn credential_relative_path(self) -> &'static str {
        match self {
            Self::CodexHome => "auth.json",
            Self::ClaudeFile => ".credentials.json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStoreMode {
    File,
    Keyring,
    Auto,
    Ephemeral,
    Unknown,
}

impl CredentialStoreMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Keyring => "keyring",
            Self::Auto => "auto",
            Self::Ephemeral => "ephemeral",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSourceRefreshOwner {
    Codex,
    ClaudeCode,
}

impl CredentialSourceRefreshOwner {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSourceHealth {
    Pending,
    Ready,
    SourceGone,
    Unreadable,
    SymlinkEscape,
    Oversized,
    PartialWrite,
    MissingFields,
    InvalidJson,
    Invalid,
    RequiresOriginClient,
    Expired,
    Revoked,
}

impl CredentialSourceHealth {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::SourceGone => "source_gone",
            Self::Unreadable => "unreadable",
            Self::SymlinkEscape => "symlink_escape",
            Self::Oversized => "oversized",
            Self::PartialWrite => "partial_write",
            Self::MissingFields => "missing_fields",
            Self::InvalidJson => "invalid_json",
            Self::Invalid => "invalid",
            Self::RequiresOriginClient => "requires_origin_client",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSourceRecord {
    pub id: String,
    pub kind: CredentialSourceKind,
    pub root: PathBuf,
    pub label: String,
    pub enabled: bool,
    pub store_mode: CredentialStoreMode,
    pub refresh_owner: CredentialSourceRefreshOwner,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scanned_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refreshed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_expires_at_ms: Option<u64>,
    pub health: CredentialSourceHealth,
}

impl CredentialSourceRecord {
    #[must_use]
    pub fn credential_path(&self) -> PathBuf {
        self.root.join(self.kind.credential_relative_path())
    }
}

#[derive(Debug, Clone)]
pub struct CredentialSourceRegistry {
    path: PathBuf,
    records: Vec<CredentialSourceRecord>,
}

impl CredentialSourceRegistry {
    pub fn load(profile_dir: impl AsRef<Path>) -> AccountsResult<Self> {
        let path = profile_dir.as_ref().join(CREDENTIAL_SOURCES_FILE_NAME);
        let records = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                accounts_error(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "credential source registry `{}` is not valid JSON: {error}",
                        path.display()
                    ),
                    false,
                )
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(file_error("read", &path, &error)),
        };
        validate_records(&records)?;
        Ok(Self { path, records })
    }

    #[must_use]
    pub fn records(&self) -> &[CredentialSourceRecord] {
        &self.records
    }

    #[must_use]
    pub fn find_by_alias(&self, alias: &str) -> Option<&CredentialSourceRecord> {
        self.records
            .iter()
            .find(|record| record.account_alias.as_deref() == Some(alias))
    }

    pub fn enroll(
        &mut self,
        kind: CredentialSourceKind,
        root: impl AsRef<Path>,
        label: Option<&str>,
    ) -> AccountsResult<CredentialSourceRecord> {
        let root = canonical_enrollment_root(root.as_ref())?;
        self.upsert(kind, root, label)
    }

    /// Registers a conventional platform default even before its directory is
    /// created. Unlike operator enrollment, this accepts only a normalized
    /// absolute path assembled by Haider itself.
    pub fn ensure_default(
        &mut self,
        kind: CredentialSourceKind,
        root: impl AsRef<Path>,
        label: &str,
    ) -> AccountsResult<CredentialSourceRecord> {
        let root = normalize_absolute(root.as_ref())?;
        if let Some(existing) = self
            .records
            .iter()
            .find(|record| record.kind == kind && record.root == root)
        {
            // An operator tombstone beats automatic default discovery. Only
            // an explicit `enroll` may revive a disabled source.
            return Ok(existing.clone());
        }
        self.upsert(kind, root, Some(label))
    }

    fn upsert(
        &mut self,
        kind: CredentialSourceKind,
        root: PathBuf,
        label: Option<&str>,
    ) -> AccountsResult<CredentialSourceRecord> {
        if let Some(index) = self
            .records
            .iter()
            .position(|record| record.kind == kind && record.root == root)
        {
            let existing = &self.records[index];
            if existing.enabled {
                return Ok(existing.clone());
            }
            let mut next = self.records.clone();
            next[index].enabled = true;
            next[index].health = CredentialSourceHealth::Pending;
            next[index].last_scanned_at_ms = None;
            if let Some(label) = sanitized_label(label) {
                next[index].label = label;
            }
            let reenrolled = next[index].clone();
            self.commit(next)?;
            return Ok(reenrolled);
        }
        if self.records.len() >= MAX_SOURCES {
            return Err(accounts_error(
                ErrorCode::InvalidArgument,
                format!("at most {MAX_SOURCES} credential sources may be enrolled"),
                false,
            ));
        }
        let id = source_id(kind, &root);
        let record = CredentialSourceRecord {
            id,
            kind,
            root,
            label: sanitized_label(label).unwrap_or_else(|| kind.as_str().replace('_', " ")),
            enabled: true,
            store_mode: CredentialStoreMode::Unknown,
            refresh_owner: match kind {
                CredentialSourceKind::CodexHome => CredentialSourceRefreshOwner::Codex,
                CredentialSourceKind::ClaudeFile => CredentialSourceRefreshOwner::ClaudeCode,
            },
            account_alias: None,
            last_scanned_at_ms: None,
            last_refreshed_at_ms: None,
            access_expires_at_ms: None,
            health: CredentialSourceHealth::Pending,
        };
        let mut next = self.records.clone();
        next.push(record.clone());
        next.sort_by(|left, right| left.id.cmp(&right.id));
        self.commit(next)?;
        Ok(record)
    }

    pub fn update(&mut self, updated: CredentialSourceRecord) -> AccountsResult<()> {
        let Some(index) = self
            .records
            .iter()
            .position(|record| record.id == updated.id)
        else {
            return Err(accounts_error(
                ErrorCode::CredentialMissing,
                format!("credential source `{}` does not exist", updated.id),
                false,
            ));
        };
        if self.records[index].kind != updated.kind || self.records[index].root != updated.root {
            return Err(accounts_error(
                ErrorCode::InvalidArgument,
                "credential source identity coordinates are immutable",
                false,
            ));
        }
        let mut next = self.records.clone();
        next[index] = updated;
        self.commit(next)
    }

    pub fn remove(&mut self, id: &str) -> AccountsResult<CredentialSourceRecord> {
        let Some(index) = self.records.iter().position(|record| record.id == id) else {
            return Err(accounts_error(
                ErrorCode::CredentialMissing,
                format!("credential source `{id}` does not exist"),
                false,
            ));
        };
        let mut next = self.records.clone();
        let removed = next.remove(index);
        self.commit(next)?;
        Ok(removed)
    }

    fn commit(&mut self, records: Vec<CredentialSourceRecord>) -> AccountsResult<()> {
        validate_records(&records)?;
        let parent = self.path.parent().ok_or_else(|| {
            accounts_error(
                ErrorCode::InvalidArgument,
                "credential source registry path has no parent",
                false,
            )
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| file_error("create parent for", &self.path, &error))?;
        let bytes = serde_json::to_vec_pretty(&records).map_err(|error| {
            accounts_error(
                ErrorCode::Internal,
                format!("could not serialize credential source registry: {error}"),
                false,
            )
        })?;
        let temporary = self.path.with_extension("json.tmp");
        let mut file = open_temporary(&temporary)?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| file_error("write", &temporary, &error))?;
        drop(file);
        haider_platform::replace_file(&temporary, &self.path)
            .map_err(|error| file_error("replace", &self.path, &error))?;
        haider_platform::sync_directory(parent)
            .map_err(|error| file_error("sync parent of", &self.path, &error))?;
        self.records = records;
        Ok(())
    }
}

fn validate_records(records: &[CredentialSourceRecord]) -> AccountsResult<()> {
    if records.len() > MAX_SOURCES {
        return Err(accounts_error(
            ErrorCode::StoreCorrupt,
            "credential source registry exceeds its source limit",
            false,
        ));
    }
    let mut ids = std::collections::HashSet::new();
    let mut coordinates = std::collections::HashSet::new();
    let mut aliases = std::collections::HashSet::new();
    for record in records {
        if record.id != source_id(record.kind, &record.root)
            || !record.root.is_absolute()
            || record.label.is_empty()
            || !ids.insert(record.id.as_str())
            || !coordinates.insert((record.kind, record.root.as_path()))
            || record
                .account_alias
                .as_deref()
                .is_some_and(|alias| alias.is_empty() || !aliases.insert(alias))
        {
            return Err(accounts_error(
                ErrorCode::StoreCorrupt,
                "credential source registry contains invalid or duplicate coordinates",
                false,
            ));
        }
    }
    Ok(())
}

fn canonical_enrollment_root(path: &Path) -> AccountsResult<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        accounts_error(
            ErrorCode::InvalidArgument,
            format!(
                "credential source root `{}` cannot be enrolled: {error}",
                path.display()
            ),
            false,
        )
    })?;
    if !canonical.is_dir() {
        return Err(accounts_error(
            ErrorCode::InvalidArgument,
            format!(
                "credential source root `{}` is not a directory",
                canonical.display()
            ),
            false,
        ));
    }
    normalize_absolute(&canonical)
}

fn normalize_absolute(path: &Path) -> AccountsResult<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(accounts_error(
            ErrorCode::InvalidArgument,
            "credential source root must be a normalized absolute path",
            false,
        ));
    }
    Ok(path.to_path_buf())
}

fn source_id(kind: CredentialSourceKind, root: &Path) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"haider-credential-source-v1\0");
    hash.update(kind.as_str().as_bytes());
    hash.update(b"\0");
    hash.update(root.as_os_str().as_encoded_bytes());
    format!("src1_{}", hash.finalize().to_hex())
}

fn sanitized_label(label: Option<&str>) -> Option<String> {
    let label = label?;
    let value = label
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn open_temporary(path: &Path) -> AccountsResult<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| file_error("open temporary", path, &error))
}

fn file_error(
    action: &str,
    path: &Path,
    error: &std::io::Error,
) -> haider_protocol::error::HaiderError {
    accounts_error(
        ErrorCode::Internal,
        format!(
            "could not {action} credential source `{}`: {error}",
            path.display()
        ),
        error.kind() == std::io::ErrorKind::Interrupted,
    )
}

#[cfg(test)]
#[path = "source_registry_tests.rs"]
mod tests;
