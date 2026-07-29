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
use std::sync::Mutex;

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

impl SecretHandle {
    /// Crate-private on purpose: only [`Vault`] implementations mint handles.
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
