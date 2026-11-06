//! Durable row keys for the `provider_models` catalog cache.
//!
//! Public (credential-free) catalogs are keyed by the bare provider ID, as
//! they were before lane 973-catalog-accuracy. Authenticated catalogs are
//! keyed by the account identity that fetched them, so another identity —
//! including one later reusing the same alias or the same credential source —
//! never reads them. A proven same-account credential refresh or a daemon
//! restart derives the identical key; an unproven intake mints a new key.

use haider_protocol::credential::{AuthMethod, CredentialDescriptor};

/// Prefix of account-scoped keys. `v3` adds a durable daemon-minted
/// incarnation; startup pruning removes unread v2 and older keys.
/// `v2` (lane 973-catalog-accuracy, repair 2)
/// replaced the unreleased alias-only `account:<len>:<provider>:<alias>`
/// form; those rows and pre-973 provider-only authenticated rows are unread
/// and removed by the catalog prune on startup or account mutation.
pub(crate) const ACCOUNT_SCOPED_PREFIX: &str = "account-v3:";

/// A `provider_models` row key. Construct it only through [`Self::public`]
/// or [`Self::for_account`] so every reader and writer derives it the same way.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProviderModelCacheKey(String);

impl ProviderModelCacheKey {
    /// Key for a catalog whose discovery needs no credential.
    #[must_use]
    pub fn public(provider: &str) -> Self {
        Self(provider.to_owned())
    }

    /// Key for an authenticated catalog fetched by `descriptor`'s account.
    ///
    /// The scope includes every present account ID and email. Hashing keeps
    /// those public identity fields out of
    /// SQLite's key column. The material layout is persisted and pinned by
    /// `account_scoped_cache_key_derivation_is_pinned`; changing it makes
    /// every existing account catalog cold.
    #[must_use]
    pub fn for_account(provider: &str, descriptor: &CredentialDescriptor) -> Self {
        let identity = descriptor.account_identity.as_ref();
        let issuer = identity
            .and_then(|identity| identity.issuer.as_deref())
            .unwrap_or("");
        let account_id = identity
            .and_then(|identity| identity.account_id.as_deref())
            .unwrap_or("");
        let email = identity
            .and_then(|identity| identity.email.as_deref())
            .unwrap_or("");
        let incarnation = descriptor.incarnation.to_string();
        let fields = [
            provider,
            descriptor.alias.as_str(),
            auth_method_tag(descriptor.auth_method),
            incarnation.as_str(),
            descriptor.base_url.as_deref().unwrap_or(""),
            issuer,
            account_id,
            email,
        ];
        let mut material = blake3::Hasher::new();
        for field in fields {
            material.update(&(field.len() as u64).to_le_bytes());
            material.update(field.as_bytes());
        }
        Self(format!(
            "{ACCOUNT_SCOPED_PREFIX}{}",
            material.finalize().to_hex()
        ))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<ProviderModelCacheKey> for String {
    fn from(key: ProviderModelCacheKey) -> Self {
        key.0
    }
}

impl std::fmt::Display for ProviderModelCacheKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Persisted spelling of the auth method inside key material. These are the
/// enum's `Debug` names at the time v2 keys were introduced; spelled out so a
/// variant rename cannot silently re-key every cached catalog.
fn auth_method_tag(auth_method: AuthMethod) -> &'static str {
    match auth_method {
        AuthMethod::ApiKey => "ApiKey",
        AuthMethod::OAuth => "OAuth",
    }
}
