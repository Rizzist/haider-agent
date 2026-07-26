//! Persisted account descriptor collection and its invariants.
//!
//! Owned invariant — ONE ACTIVE PER PROVIDER: every provider with at least
//! one descriptor has exactly one active descriptor. `AccountStore` builds
//! each mutation as a candidate snapshot, revalidates the whole snapshot, and
//! persists it before the in-memory view changes — so a failed save leaves
//! both the view and the file untouched, and a snapshot that violates the
//! invariant is rejected on load as `StoreCorrupt`.
//!
//! Descriptors are display metadata only; secret bytes live behind `Vault`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use haider_protocol::credential::CredentialDescriptor;
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;

use crate::{AccountsResult, accounts_error};

/// Profile-relative JSON file containing credential descriptors.
pub const ACCOUNTS_FILE_NAME: &str = "accounts.json";

/// Persistence port for account descriptor snapshots.
pub trait StoreLike: Send + Sync {
    /// Loads the entire descriptor snapshot.
    fn load(&self) -> AccountsResult<Vec<CredentialDescriptor>>;

    /// Atomically replaces the entire descriptor snapshot.
    fn save(&self, descriptors: &[CredentialDescriptor]) -> AccountsResult<()>;
}

/// JSON account snapshot stored beside the profile's other metadata.
#[derive(Debug, Clone)]
pub struct JsonFileStore {
    path: PathBuf,
}

impl JsonFileStore {
    /// Creates a store whose file is `<profile_dir>/accounts.json`.
    #[must_use]
    pub fn new(profile_dir: impl AsRef<Path>) -> Self {
        Self {
            path: profile_dir.as_ref().join(ACCOUNTS_FILE_NAME),
        }
    }

    /// Returns the descriptor file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl StoreLike for JsonFileStore {
    fn load(&self) -> AccountsResult<Vec<CredentialDescriptor>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(file_error("read", &self.path, error)),
        };

        serde_json::from_slice(&bytes).map_err(|error| {
            accounts_error(
                ErrorCode::StoreCorrupt,
                format!(
                    "account descriptor file `{}` is not valid JSON: {error}",
                    self.path.display()
                ),
                false,
            )
        })
    }

    fn save(&self, descriptors: &[CredentialDescriptor]) -> AccountsResult<()> {
        let parent = self.path.parent().ok_or_else(|| {
            accounts_error(
                ErrorCode::InvalidArgument,
                format!(
                    "account descriptor path `{}` has no parent directory",
                    self.path.display()
                ),
                false,
            )
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| file_error("create parent directory for", &self.path, error))?;

        let bytes = serde_json::to_vec_pretty(descriptors).map_err(|error| {
            accounts_error(
                ErrorCode::Internal,
                format!("could not serialize account descriptors: {error}"),
                false,
            )
        })?;
        // Write-then-rename keeps the snapshot replacement atomic. The fixed
        // temporary name assumes one writer per profile directory;
        // cross-process locking arrives with the daemon integration.
        let temporary_path = self.path.with_extension("json.tmp");
        let mut temporary = open_temporary(&temporary_path)?;
        temporary
            .write_all(&bytes)
            .and_then(|()| temporary.write_all(b"\n"))
            .and_then(|()| temporary.sync_all())
            .map_err(|error| file_error("write", &temporary_path, error))?;
        drop(temporary);
        fs::rename(&temporary_path, &self.path)
            .map_err(|error| file_error("replace", &self.path, error))
    }
}

fn open_temporary(path: &Path) -> AccountsResult<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| file_error("open temporary file for", path, error))
}

fn file_error(
    action: &str,
    path: &Path,
    error: std::io::Error,
) -> haider_protocol::error::HaiderError {
    accounts_error(
        ErrorCode::Internal,
        format!(
            "could not {action} account descriptor file `{}`: {error}",
            path.display()
        ),
        error.kind() == std::io::ErrorKind::Interrupted,
    )
}

/// In-memory account view backed by a whole-snapshot persistence port.
pub struct AccountStore<S> {
    store: S,
    descriptors: Vec<CredentialDescriptor>,
}

impl<S: StoreLike> AccountStore<S> {
    /// Loads and validates descriptors from `store`.
    pub fn new(store: S) -> AccountsResult<Self> {
        let descriptors = store.load()?;
        validate_snapshot(&descriptors)?;
        Ok(Self { store, descriptors })
    }

    /// Adds a descriptor while preserving one active account per provider.
    ///
    /// The first account for a provider becomes active. Adding a later active
    /// account deselects the previous active account.
    pub fn add(&mut self, mut descriptor: CredentialDescriptor) -> AccountsResult<()> {
        validate_descriptor(&descriptor)?;
        if self
            .descriptors
            .iter()
            .any(|existing| existing.alias == descriptor.alias)
        {
            return Err(accounts_error(
                ErrorCode::InvalidArgument,
                format!("credential alias `{}` already exists", descriptor.alias),
                false,
            ));
        }

        let provider_exists = self
            .descriptors
            .iter()
            .any(|existing| existing.provider == descriptor.provider);
        if !provider_exists {
            descriptor.active = true;
        }

        let mut next = self.descriptors.clone();
        if descriptor.active {
            for existing in &mut next {
                if existing.provider == descriptor.provider {
                    existing.active = false;
                }
            }
        }
        next.push(descriptor);
        self.commit(next)
    }

    /// Makes `alias` the active account for its provider.
    pub fn select(&mut self, alias: &CredentialAlias) -> AccountsResult<()> {
        let provider = self
            .descriptors
            .iter()
            .find(|descriptor| descriptor.alias == *alias)
            .map(|descriptor| descriptor.provider.clone())
            .ok_or_else(|| missing_alias(alias))?;

        let mut next = self.descriptors.clone();
        for descriptor in &mut next {
            if descriptor.provider == provider {
                descriptor.active = descriptor.alias == *alias;
            }
        }
        self.commit(next)
    }

    /// Removes `alias`, selecting the provider's first remaining account when
    /// the active account was removed.
    pub fn remove(&mut self, alias: &CredentialAlias) -> AccountsResult<CredentialDescriptor> {
        let index = self
            .descriptors
            .iter()
            .position(|descriptor| descriptor.alias == *alias)
            .ok_or_else(|| missing_alias(alias))?;
        let removed = self.descriptors[index].clone();
        let mut next = self.descriptors.clone();
        next.remove(index);

        if removed.active
            && let Some(successor) = next
                .iter_mut()
                .find(|descriptor| descriptor.provider == removed.provider)
        {
            successor.active = true;
        }
        self.commit(next)?;
        Ok(removed)
    }

    /// Returns all descriptors in stable insertion order.
    #[must_use]
    pub fn list(&self) -> &[CredentialDescriptor] {
        &self.descriptors
    }

    /// Returns the descriptor for `alias`.
    #[must_use]
    pub fn get(&self, alias: &CredentialAlias) -> Option<&CredentialDescriptor> {
        self.descriptors
            .iter()
            .find(|descriptor| descriptor.alias == *alias)
    }

    /// Returns the active descriptor for `provider`.
    #[must_use]
    pub fn active_for_provider(&self, provider: &str) -> Option<&CredentialDescriptor> {
        self.descriptors
            .iter()
            .find(|descriptor| descriptor.provider == provider && descriptor.active)
    }

    fn commit(&mut self, next: Vec<CredentialDescriptor>) -> AccountsResult<()> {
        validate_snapshot(&next)?;
        self.store.save(&next)?;
        self.descriptors = next;
        Ok(())
    }
}

/// Rejects (as `StoreCorrupt`) invalid descriptors, duplicate aliases, and any
/// provider whose active count is not exactly one.
fn validate_snapshot(descriptors: &[CredentialDescriptor]) -> AccountsResult<()> {
    let mut aliases = HashSet::new();
    let mut active_counts: HashMap<&str, usize> = HashMap::new();

    for descriptor in descriptors {
        validate_descriptor(descriptor).map_err(|error| {
            accounts_error(
                ErrorCode::StoreCorrupt,
                format!("invalid persisted account descriptor: {}", error.message),
                false,
            )
        })?;
        if !aliases.insert(descriptor.alias.as_str()) {
            return Err(accounts_error(
                ErrorCode::StoreCorrupt,
                format!(
                    "persisted account descriptors contain duplicate alias `{}`",
                    descriptor.alias
                ),
                false,
            ));
        }
        *active_counts.entry(&descriptor.provider).or_insert(0) += usize::from(descriptor.active);
    }

    for (provider, active) in active_counts {
        if active != 1 {
            return Err(accounts_error(
                ErrorCode::StoreCorrupt,
                format!(
                    "provider `{provider}` has {active} active accounts; exactly one is required"
                ),
                false,
            ));
        }
    }
    Ok(())
}

/// Rejects a descriptor with an empty alias or provider.
fn validate_descriptor(descriptor: &CredentialDescriptor) -> AccountsResult<()> {
    if descriptor.alias.as_str().is_empty() {
        return Err(accounts_error(
            ErrorCode::InvalidArgument,
            "credential alias cannot be empty",
            false,
        ));
    }
    if descriptor.provider.is_empty() {
        return Err(accounts_error(
            ErrorCode::InvalidArgument,
            "credential provider cannot be empty",
            false,
        ));
    }
    Ok(())
}

fn missing_alias(alias: &CredentialAlias) -> haider_protocol::error::HaiderError {
    accounts_error(
        ErrorCode::CredentialMissing,
        format!("credential alias `{alias}` does not exist"),
        false,
    )
}
