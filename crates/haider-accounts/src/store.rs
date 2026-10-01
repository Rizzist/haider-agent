//! Persisted account descriptor collection and its invariants.
//!
//! Owned invariant — ONE ACTIVE PER PROVIDER: every provider with at least
//! one descriptor has exactly one active descriptor. `AccountStore` builds
//! each mutation as a candidate snapshot, revalidates the whole snapshot, and
//! persists it before the in-memory view changes — so a failed save leaves
//! both the view and the file untouched, and a snapshot that violates the
//! invariant is rejected on load as `StoreCorrupt`.
//!
//! Descriptors contain only secret-free metadata and catalog coordinates;
//! secret bytes live behind `Vault`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use haider_protocol::credential::{AccountIdentity, CredentialDescriptor, CredentialStatus};
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

    /// Reserve an incarnation before committing a descriptor. Gaps after a
    /// failed descriptor write are intentional: a value is never recycled.
    fn reserve_incarnation(&self, floor: u64) -> AccountsResult<u64>;
}

/// JSON account snapshot stored beside the profile's other metadata.
#[derive(Debug, Clone)]
pub struct JsonFileStore {
    path: PathBuf,
    #[cfg(test)]
    pub(crate) counter_sync: crate::store_tests::CounterSyncProbe,
}

impl JsonFileStore {
    /// Creates a store whose file is `<profile_dir>/accounts.json`.
    #[must_use]
    pub fn new(profile_dir: impl AsRef<Path>) -> Self {
        Self {
            path: profile_dir.as_ref().join(ACCOUNTS_FILE_NAME),
            #[cfg(test)]
            counter_sync: Default::default(),
        }
    }

    /// Returns the descriptor file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn sync_counter_file(&self, file: &File) -> std::io::Result<()> {
        file.sync_all()?;
        #[cfg(test)]
        self.counter_sync
            .record(crate::store_tests::CounterSyncStage::File)?;
        Ok(())
    }

    pub(crate) fn sync_counter_directory(&self, parent: &Path) -> std::io::Result<()> {
        haider_platform::sync_directory(parent)?;
        #[cfg(test)]
        self.counter_sync
            .record(crate::store_tests::CounterSyncStage::Directory)?;
        Ok(())
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
        #[cfg(test)]
        self.counter_sync
            .record(crate::store_tests::CounterSyncStage::Descriptor)
            .map_err(|error| file_error("sync", &self.path, error))?;
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
        haider_platform::replace_file(&temporary_path, &self.path)
            .map_err(|error| file_error("replace", &self.path, error))?;
        // R10 hardening: the rename is durable only once the PARENT
        // DIRECTORY is synced — the pending-login receipt reconciliation
        // protocol relies on a post-crash descriptor file reflecting every
        // acknowledged save.
        haider_platform::sync_directory(parent)
            .map_err(|error| file_error("sync parent directory of", &self.path, error))
    }

    fn reserve_incarnation(&self, floor: u64) -> AccountsResult<u64> {
        // The profile daemon is the sole writer of this account store. Keep
        // the counter in a separate fsynced file so a removed last alias or
        // an interrupted descriptor commit cannot recycle its coordinate.
        let path = self.path.with_extension("incarnation-counter");
        let current = match fs::read_to_string(&path) {
            Ok(value) => value.trim().parse::<u64>().map_err(|_| {
                accounts_error(
                    ErrorCode::StoreCorrupt,
                    "invalid account incarnation counter",
                    false,
                )
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(file_error("read", &path, error)),
        };
        let next = current.max(floor).checked_add(1).ok_or_else(|| {
            accounts_error(ErrorCode::Internal, "account incarnation exhausted", false)
        })?;
        let parent = path.parent().ok_or_else(|| {
            accounts_error(
                ErrorCode::InvalidArgument,
                "account counter has no parent",
                false,
            )
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| file_error("create parent for", &path, error))?;
        let temporary_path = path.with_extension("incarnation-counter.tmp");
        let mut temporary = open_temporary(&temporary_path)?;
        temporary
            .write_all(next.to_string().as_bytes())
            .and_then(|()| temporary.write_all(b"\n"))
            .and_then(|()| self.sync_counter_file(&temporary))
            .map_err(|error| file_error("write", &temporary_path, error))?;
        drop(temporary);
        haider_platform::replace_file(&temporary_path, &path)
            .map_err(|error| file_error("replace", &path, error))?;
        self.sync_counter_directory(parent)
            .map_err(|error| file_error("sync parent directory of", &path, error))?;
        Ok(next)
    }
}

/// Boxed stores forward verbatim, so the daemon can inject test persistence
/// behind one owned `AccountStore<Box<dyn StoreLike>>`.
impl StoreLike for Box<dyn StoreLike> {
    fn load(&self) -> AccountsResult<Vec<CredentialDescriptor>> {
        self.as_ref().load()
    }

    fn save(&self, descriptors: &[CredentialDescriptor]) -> AccountsResult<()> {
        self.as_ref().save(descriptors)
    }

    fn reserve_incarnation(&self, floor: u64) -> AccountsResult<u64> {
        self.as_ref().reserve_incarnation(floor)
    }
}

/// Shared stores forward verbatim (cloneable injection seam).
impl StoreLike for std::sync::Arc<dyn StoreLike> {
    fn load(&self) -> AccountsResult<Vec<CredentialDescriptor>> {
        self.as_ref().load()
    }

    fn save(&self, descriptors: &[CredentialDescriptor]) -> AccountsResult<()> {
        self.as_ref().save(descriptors)
    }

    fn reserve_incarnation(&self, floor: u64) -> AccountsResult<u64> {
        self.as_ref().reserve_incarnation(floor)
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

/// Only provider-issued account coordinates or identical ingested bytes
/// establish continuity. Labels, source paths and timestamps never do.
fn proven_same_account(previous: &CredentialDescriptor, next: &CredentialDescriptor) -> bool {
    if previous.provider != next.provider
        || previous.auth_method != next.auth_method
        || previous.base_url != next.base_url
    {
        return false;
    }
    if let (Some(a), Some(b)) = (&previous.credential_digest, &next.credential_digest)
        && a == b
    {
        return true;
    }
    if identity_coordinates_conflict(previous, next) {
        return false;
    }
    if previous.auth_method == haider_protocol::credential::AuthMethod::ApiKey {
        return false;
    }
    let (Some(a), Some(b)) = (&previous.account_identity, &next.account_identity) else {
        return false;
    };
    if a.issuer != b.issuer || !a.issuer.as_deref().is_some_and(complete_coordinate) {
        return false;
    }
    let id = a.account_id.as_deref().filter(|id| complete_coordinate(id));
    if let Some(id) = id
        && b.account_id.as_deref() == Some(id)
    {
        if provider_user_coordinate(id) {
            return true;
        }
        if a.email.as_deref().is_some_and(complete_coordinate) && a.email == b.email {
            return true;
        }
        if a.verified && b.verified && previous.provider != "openai-oauth" {
            return true;
        }
    }
    a.verified
        && b.verified
        && a.email.as_deref().is_some_and(complete_coordinate)
        && a.email == b.email
}

/// An owned grant's synthetic coordinate survives token rotation. This
/// exception is ONLY for manual identity reconciliation; new imports and
/// re-logins still pass the stricter intake continuity rule.
fn unchanged_owned_grant(previous: &CredentialDescriptor, next: &CredentialDescriptor) -> bool {
    let (Some(a), Some(b)) = (&previous.account_identity, &next.account_identity) else {
        return false;
    };
    previous.auth_method == haider_protocol::credential::AuthMethod::OAuth
        && matches!(
            previous.provider.as_str(),
            "kimi-oauth" | "grok-oauth" | "anthropic-oauth"
        )
        && previous.provider == next.provider
        && previous.base_url == next.base_url
        && a.issuer == b.issuer
        && a.issuer.as_deref().is_some_and(complete_coordinate)
        && a.account_id == b.account_id
        && a.account_id.as_deref().is_some_and(|id| {
            let prefix = if previous.provider == "anthropic-oauth" {
                "anthropic:".to_owned()
            } else {
                format!("{}:grant-", previous.provider)
            };
            complete_coordinate(id)
                && id.strip_prefix(&prefix).is_some_and(|grant| {
                    grant.len() == 16 && grant.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        })
        && !identity_coordinates_conflict(previous, next)
}

fn complete_coordinate(value: &str) -> bool {
    // AccountIdentity::sanitized_field caps fields at this length. An
    // exactly-full coordinate may have lost a distinguishing suffix.
    !value.trim().is_empty() && value.chars().count() < AccountIdentity::MAX_FIELD_CHARS
}

fn provider_user_coordinate(id: &str) -> bool {
    ["user:", "xai:"].into_iter().any(|prefix| {
        id.strip_prefix(prefix)
            .is_some_and(|value| complete_coordinate(id) && !value.trim().is_empty())
    })
}

fn identity_coordinates_conflict(
    previous: &CredentialDescriptor,
    next: &CredentialDescriptor,
) -> bool {
    let (Some(a), Some(b)) = (&previous.account_identity, &next.account_identity) else {
        return false;
    };
    if a.issuer == b.issuer
        && a.account_id == b.account_id
        && a.account_id
            .as_deref()
            .is_some_and(provider_user_coordinate)
    {
        return false;
    }
    a.issuer != b.issuer
        || matches!((&a.account_id, &b.account_id), (Some(a), Some(b)) if a != b)
        || matches!((&a.email, &b.email), (Some(a), Some(b)) if a != b)
}

fn identity_scope_changed(previous: &CredentialDescriptor, next: &CredentialDescriptor) -> bool {
    previous
        .account_identity
        .as_ref()
        .map(|identity| (&identity.issuer, &identity.account_id, &identity.email))
        != next
            .account_identity
            .as_ref()
            .map(|identity| (&identity.issuer, &identity.account_id, &identity.email))
}

/// In-memory account view backed by a whole-snapshot persistence port.
pub struct AccountStore<S> {
    store: S,
    descriptors: Vec<CredentialDescriptor>,
}

impl<S: StoreLike> AccountStore<S> {
    /// Validates a transient read-only projection used only for credential
    /// resolution. The owning daemon has already migrated its durable store;
    /// this view must never try to persist an older synthetic snapshot.
    pub fn new_read_only(store: S) -> AccountsResult<Self> {
        let descriptors = store.load()?;
        validate_snapshot(&descriptors)?;
        Ok(Self { store, descriptors })
    }

    /// Loads and validates descriptors from `store`.
    pub fn new(store: S) -> AccountsResult<Self> {
        let mut descriptors = store.load()?;
        validate_snapshot(&descriptors)?;
        // Old descriptor arrays have no incarnation. Assign one to every
        // alias before any caller can derive a v3 catalog key from the row.
        if descriptors
            .iter()
            .any(|descriptor| descriptor.incarnation == 0)
        {
            let mut floor = descriptors
                .iter()
                .map(|descriptor| descriptor.incarnation)
                .max()
                .unwrap_or(0);
            for descriptor in &mut descriptors {
                if descriptor.incarnation == 0 {
                    floor = store.reserve_incarnation(floor)?;
                    descriptor.incarnation = floor;
                }
            }
            store.save(&descriptors)?;
        }
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
        if descriptor.created_at_ms.is_none() {
            descriptor.created_at_ms = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| {
                        accounts_error(
                            ErrorCode::Internal,
                            "cannot timestamp account creation",
                            true,
                        )
                    })?
                    .as_millis()
                    .try_into()
                    .map_err(|_| {
                        accounts_error(
                            ErrorCode::Internal,
                            "account creation timestamp is out of range",
                            false,
                        )
                    })?,
            );
        }

        descriptor.incarnation = self.next_incarnation()?;

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

    /// Sets or clears one credential's operator-chosen display label.
    ///
    /// Distinct from [`Self::replace`] on purpose: `replace` rebuilds a
    /// descriptor from provider truth (OAuth rotation, re-login) and
    /// therefore PRESERVES an existing label when the incoming one is absent.
    /// This door is the one place absence means "clear it", so the two
    /// intents can never be confused.
    pub fn set_label(
        &mut self,
        alias: &CredentialAlias,
        label: Option<String>,
    ) -> AccountsResult<CredentialDescriptor> {
        let index = self
            .descriptors
            .iter()
            .position(|existing| &existing.alias == alias)
            .ok_or_else(|| missing_alias(alias))?;
        let mut next = self.descriptors.clone();
        next[index].label = label;
        let updated = next[index].clone();
        self.store.save(&next)?;
        self.descriptors = next;
        Ok(updated)
    }

    /// Persists an informational identity only when the pre-964 row has none.
    /// The creation timestamp is intentionally untouched: migration must not
    /// invent when an older account was added.
    pub fn backfill_identity(
        &mut self,
        alias: &CredentialAlias,
        identity: AccountIdentity,
    ) -> AccountsResult<bool> {
        let index = self
            .descriptors
            .iter()
            .position(|existing| &existing.alias == alias)
            .ok_or_else(|| missing_alias(alias))?;
        if self.descriptors[index].account_identity.is_some() {
            return Ok(false);
        }
        let mut next = self.descriptors.clone();
        next[index].account_identity = Some(identity);
        self.store.save(&next)?;
        self.descriptors = next;
        Ok(true)
    }

    /// Replaces only the informational identity for an existing account.
    /// Credential coordinates and the original creation time remain fixed.
    pub fn set_identity(
        &mut self,
        alias: &CredentialAlias,
        identity: Option<AccountIdentity>,
    ) -> AccountsResult<CredentialDescriptor> {
        let index = self
            .descriptors
            .iter()
            .position(|existing| &existing.alias == alias)
            .ok_or_else(|| missing_alias(alias))?;
        let mut next = self.descriptors.clone();
        next[index].account_identity = identity;
        let updated = next[index].clone();
        self.store.save(&next)?;
        self.descriptors = next;
        Ok(updated)
    }

    /// Manual identity reconciliation. A changed or unprovable account
    /// coordinate starts a new catalog generation, even when the alias and
    /// source timestamp are unchanged.
    pub fn reconcile_identity(
        &mut self,
        alias: &CredentialAlias,
        identity: Option<AccountIdentity>,
        credential_digest: Option<String>,
    ) -> AccountsResult<CredentialDescriptor> {
        let index = self
            .descriptors
            .iter()
            .position(|existing| &existing.alias == alias)
            .ok_or_else(|| missing_alias(alias))?;
        let previous = &self.descriptors[index];
        let mut next = self.descriptors.clone();
        next[index].account_identity = identity;
        next[index].credential_digest = credential_digest;
        if identity_scope_changed(previous, &next[index])
            || !(proven_same_account(previous, &next[index])
                || unchanged_owned_grant(previous, &next[index]))
        {
            next[index].incarnation = self.next_incarnation()?;
        }
        let updated = next[index].clone();
        self.store.save(&next)?;
        self.descriptors = next;
        Ok(updated)
    }

    /// A removed source may later be enrolled at the same path and alias;
    /// reserve its next incarnation before it can be observed again.
    pub fn invalidate_incarnation(&mut self, alias: &CredentialAlias) -> AccountsResult<()> {
        self.bump_incarnation(alias, true)
    }

    /// A profile configuration boundary changes the serving account context
    /// while retaining its credential. Keep the digest so an identical key
    /// entered later still proves continuity within the new context.
    pub fn mint_incarnation(&mut self, alias: &CredentialAlias) -> AccountsResult<()> {
        self.bump_incarnation(alias, false)
    }

    fn bump_incarnation(
        &mut self,
        alias: &CredentialAlias,
        forget_credential: bool,
    ) -> AccountsResult<()> {
        let index = self
            .descriptors
            .iter()
            .position(|descriptor| &descriptor.alias == alias)
            .ok_or_else(|| missing_alias(alias))?;
        let mut next = self.descriptors.clone();
        next[index].incarnation = self.next_incarnation()?;
        if forget_credential {
            next[index].credential_digest = None;
        }
        self.commit(next)
    }

    /// Atomically replaces one descriptor without changing its active slot.
    ///
    /// OAuth token rotation uses this after its new bundle is durable. The
    /// alias, provider, auth method, and base URL are immutable coordinates;
    /// replacement may update only public identity/status metadata.
    pub fn replace(&mut self, mut descriptor: CredentialDescriptor) -> AccountsResult<()> {
        validate_descriptor(&descriptor)?;
        let index = self
            .descriptors
            .iter()
            .position(|existing| existing.alias == descriptor.alias)
            .ok_or_else(|| missing_alias(&descriptor.alias))?;
        let previous = &self.descriptors[index];
        if previous.provider != descriptor.provider {
            return Err(accounts_error(
                ErrorCode::InvalidArgument,
                format!(
                    "credential alias `{}` belongs to provider `{}`, not `{}`",
                    descriptor.alias, previous.provider, descriptor.provider
                ),
                false,
            ));
        }
        if previous.auth_method != descriptor.auth_method
            || previous.base_url != descriptor.base_url
        {
            return Err(accounts_error(
                ErrorCode::InvalidArgument,
                format!(
                    "credential alias `{}` cannot change authentication coordinates during replacement",
                    descriptor.alias
                ),
                false,
            ));
        }
        descriptor.active = previous.active;
        // The operator's display label survives replacement (v0.0.938): OAuth
        // rotation and re-login rebuild a descriptor from provider truth and
        // carry no label, and silently discarding the chosen name on every
        // re-authentication would be its own small betrayal. An incoming
        // label wins when one is supplied — that is how the label door sets
        // and clears it.
        if descriptor.label.is_none() {
            descriptor.label = previous.label.clone();
        }
        // Creation is the first Haider commit, not the latest refresh or
        // re-login. Pre-964 rows deliberately keep their unknown timestamp.
        descriptor.created_at_ms = previous.created_at_ms;
        descriptor.incarnation = if proven_same_account(previous, &descriptor) {
            previous.incarnation
        } else {
            self.next_incarnation()?
        };
        let mut next = self.descriptors.clone();
        next[index] = descriptor;
        self.commit(next)
    }

    fn next_incarnation(&self) -> AccountsResult<u64> {
        self.store.reserve_incarnation(
            self.descriptors
                .iter()
                .map(|descriptor| descriptor.incarnation)
                .max()
                .unwrap_or(0),
        )
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

    /// Durably changes only the public usability status of `alias`.
    ///
    /// OAuth refresh completion uses this actor-owned mutation; callers may
    /// never edit the descriptor snapshot in place.
    pub fn set_status(
        &mut self,
        alias: &CredentialAlias,
        status: CredentialStatus,
    ) -> AccountsResult<()> {
        let mut next = self.descriptors.clone();
        let descriptor = next
            .iter_mut()
            .find(|descriptor| descriptor.alias == *alias)
            .ok_or_else(|| missing_alias(alias))?;
        descriptor.status = status;
        self.commit(next)
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
