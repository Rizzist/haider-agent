//! Profile-scoped vault namespacing (R10).
//!
//! The macOS Keychain stores every Haider credential under one machine-global
//! service, keyed by alias alone. W5c.2b made the user-facing GLOBAL alias the
//! descriptor coordinate — which is right for management UX, but the raw
//! global alias must never be the Keychain key: two profiles that both create
//! alias `work` would share one item, so the second profile's login would
//! clobber the first profile's secret and a remove in one profile would
//! delete the item the other still references.
//!
//! [`ProfileVault`] restores R10 at the one seam every daemon vault consumer
//! funnels through: writes go to a profile-scoped key, reads fall back to the
//! raw key for items created before this scheme (legacy physical aliases were
//! already profile-hash-namespaced, so the raw fallback cannot cross
//! profiles), and deletes clear both keys.
//!
//! Key format (durable contract, `haider-vault-key-v1`):
//! `{blake3("haider-vault-profile-v1\n" + profile_id)[..16]}::{alias}`.
//! The alias grammar (`[a-z0-9][a-z0-9._-]{0,63}`) and legacy hashed aliases
//! contain no `:`, and the profile hash is fixed-length hex, so the encoding
//! is unambiguous in both directions.

use std::sync::Arc;

use haider_accounts::{AccountsResult, SecretHandle, Vault};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;

/// Computes the profile-scoped vault key for `alias` (`haider-vault-key-v1`).
///
/// Public so integration tests can address the same physical item the daemon
/// wrote without re-deriving the scheme by hand.
#[must_use]
pub fn scoped_vault_alias(profile_id: &str, alias: &CredentialAlias) -> CredentialAlias {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider-vault-profile-v1\n");
    hasher.update(profile_id.as_bytes());
    let digest = hasher.finalize().to_hex();
    CredentialAlias::new(format!("{}::{}", &digest.as_str()[..16], alias.as_str()))
}

/// A [`Vault`] that namespaces every key by daemon profile.
pub struct ProfileVault {
    inner: Arc<dyn Vault>,
    profile_id: String,
}

impl ProfileVault {
    #[must_use]
    pub fn new(inner: Arc<dyn Vault>, profile_id: impl Into<String>) -> Self {
        Self {
            inner,
            profile_id: profile_id.into(),
        }
    }

    fn scoped(&self, alias: &CredentialAlias) -> CredentialAlias {
        scoped_vault_alias(&self.profile_id, alias)
    }
}

impl Vault for ProfileVault {
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
        self.inner.put(&self.scoped(alias), secret)
    }

    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        match self.inner.resolve(&self.scoped(alias)) {
            Ok(secret) => Ok(secret),
            // Legacy fallback: items written before v1 scoping live under the
            // raw alias. Those aliases were derived per-profile (blake3 over
            // profile + provider + command), so this read cannot observe
            // another profile's item.
            Err(error) if error.code == ErrorCode::CredentialMissing => self.inner.resolve(alias),
            Err(error) => Err(error),
        }
    }

    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
        // Both keys: the scoped item and any legacy raw item. Deleting an
        // absent alias succeeds, so this is idempotent in every combination.
        self.inner.delete(&self.scoped(alias))?;
        self.inner.delete(alias)
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        let prefix = {
            let scoped = scoped_vault_alias(&self.profile_id, &CredentialAlias::new(""));
            scoped.as_str().to_owned()
        };
        Ok(self
            .inner
            .list()?
            .into_iter()
            .filter_map(|alias| {
                let text = alias.as_str();
                if let Some(stripped) = text.strip_prefix(&prefix) {
                    Some(CredentialAlias::new(stripped))
                } else if text.contains("::") {
                    // Another profile's scoped item: never visible here.
                    None
                } else {
                    // Legacy raw item (already profile-hash-namespaced).
                    Some(alias)
                }
            })
            .collect())
    }
}

#[cfg(test)]
#[path = "profile_vault_tests.rs"]
mod profile_vault_tests;
