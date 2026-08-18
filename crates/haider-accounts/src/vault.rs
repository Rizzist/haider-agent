//! Secret storage ports and the in-memory test implementation.
//!
//! Owned invariant — SECRETS NEVER ESCAPE: secret bytes leave a [`Vault`] only
//! inside [`SecretHandle`], which cannot be cloned, serialized, or constructed
//! outside this crate, and whose `Debug`/`Display` are unconditionally
//! redacted. [`SecretHandle::expose_secret`] is the single opt-in accessor.
//! Anything else this module puts into an error or a format string is an
//! alias, never a secret.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;
use zeroize::Zeroizing;

use crate::{AccountsResult, accounts_error};

/// An owned secret whose ordinary formatting never reveals its contents.
///
/// Callers must opt in to secret access through [`Self::expose_secret`].
pub struct SecretHandle {
    secret: Zeroizing<Box<[u8]>>,
}

/// Opaque exclusive lease for a vault alias's refresh-token rotation.
///
/// The lease contains no credential bytes and releases on drop. Production
/// file vaults override [`Vault::try_refresh_lock`] with an OS file lock so
/// independent daemon processes serialize the read-refresh-persist critical
/// section. The default implementation is a process-local gate for injected
/// test vaults and alternate vault implementations.
pub struct VaultRefreshLock {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl VaultRefreshLock {
    pub(crate) fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

impl fmt::Debug for VaultRefreshLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VaultRefreshLock")
    }
}

impl Drop for VaultRefreshLock {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

impl SecretHandle {
    /// Crate-private on purpose: only [`Vault`] implementations mint handles.
    #[cfg(target_os = "macos")]
    pub(crate) fn new(secret: Vec<u8>) -> Self {
        Self {
            secret: Zeroizing::new(secret.into_boxed_slice()),
        }
    }

    pub(crate) fn from_zeroizing(secret: &Zeroizing<Vec<u8>>) -> Self {
        Self {
            secret: Zeroizing::new(secret.as_slice().into()),
        }
    }

    /// Borrows the secret bytes.
    ///
    /// Keep the returned borrow short-lived and never include it in logs,
    /// errors, or protocol values.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        &self.secret
    }
}

impl fmt::Debug for SecretHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretHandle([REDACTED])")
    }
}

impl fmt::Display for SecretHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// Storage boundary for credential secrets.
pub trait Vault: Send + Sync {
    /// Creates or replaces the secret associated with `alias`.
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()>;

    /// Resolves `alias` to an owned, formatting-safe secret handle.
    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle>;

    /// Deletes `alias`. Deleting an absent alias succeeds.
    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()>;

    /// Lists stored aliases in stable lexical order.
    fn list(&self) -> AccountsResult<Vec<CredentialAlias>>;

    /// Attempts to acquire the exclusive refresh-rotation lease for `alias`.
    ///
    /// `Ok(None)` means another refresher owns it. Implementations must not
    /// block: the daemon retries with cancellable async backoff. The default
    /// gate is process-local; durable vaults should override it with an OS
    /// lock keyed by their physical alias.
    fn try_refresh_lock(
        &self,
        alias: &CredentialAlias,
    ) -> AccountsResult<Option<VaultRefreshLock>> {
        try_local_refresh_lock(alias)
    }
}

fn try_local_refresh_lock(alias: &CredentialAlias) -> AccountsResult<Option<VaultRefreshLock>> {
    static LOCKS: OnceLock<Mutex<BTreeMap<String, Weak<AtomicBool>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let gate = {
        let mut locks = locks.lock().map_err(|_| {
            accounts_error(
                ErrorCode::Internal,
                "vault refresh-lock registry was poisoned",
                false,
            )
        })?;
        let gate = locks
            .get(alias.as_str())
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        locks.insert(alias.as_str().to_owned(), Arc::downgrade(&gate));
        gate
    };
    if gate
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(None);
    }
    Ok(Some(VaultRefreshLock::new(move || {
        gate.store(false, Ordering::Release);
    })))
}

/// Process-local vault for tests and deterministic harnesses.
///
/// Deliberately not `Debug`: the map values are raw secret bytes.
#[derive(Default)]
pub struct MemoryVault {
    secrets: Mutex<BTreeMap<String, Zeroizing<Vec<u8>>>>,
}

impl MemoryVault {
    /// Creates an empty in-memory vault.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(
        &self,
    ) -> AccountsResult<std::sync::MutexGuard<'_, BTreeMap<String, Zeroizing<Vec<u8>>>>> {
        self.secrets.lock().map_err(|_| {
            accounts_error(ErrorCode::Internal, "memory vault lock was poisoned", false)
        })
    }
}

impl Vault for MemoryVault {
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
        if let Some(previous) = self
            .lock()?
            .insert(alias.as_str().to_owned(), Zeroizing::new(secret.to_vec()))
        {
            drop(previous);
        }
        Ok(())
    }

    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        let secret = self.lock()?.get(alias.as_str()).cloned().ok_or_else(|| {
            accounts_error(
                ErrorCode::CredentialMissing,
                format!("no secret is stored for credential alias `{alias}`"),
                false,
            )
        })?;
        Ok(SecretHandle::from_zeroizing(&secret))
    }

    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
        drop(self.lock()?.remove(alias.as_str()));
        Ok(())
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        Ok(self
            .lock()?
            .keys()
            .map(|alias| CredentialAlias::new(alias.clone()))
            .collect())
    }
}

impl Drop for MemoryVault {
    fn drop(&mut self) {
        // `Zeroizing` values scrub themselves even on poisoned teardown.
        self.secrets
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}
