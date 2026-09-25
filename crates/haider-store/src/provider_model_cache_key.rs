//! Durable row keys for the `provider_models` catalog cache.
//!
//! Public (credential-free) catalogs are keyed by the bare provider ID, as
//! they were before lane 973-catalog-accuracy. Authenticated catalogs are
//! keyed by the account identity that fetched them, so another identity —
//! including one later reusing the same alias — never reads them, while a
//! same-identity re-login or daemon restart derives the identical key.

use haider_protocol::credential::{AuthMethod, CredentialDescriptor};

/// Prefix of account-scoped keys. `v2` (lane 973-catalog-accuracy, repair 2)
/// replaced the unreleased alias-only `account:<len>:<provider>:<alias>`
/// form; those rows and pre-973 provider-only authenticated rows are unread
/// and removed by the catalog prune on startup or account mutation.
pub(crate) const ACCOUNT_SCOPED_PREFIX: &str = "account-v2:";

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
    /// The identity scope prefers the stable account ID, then the email, then
    /// the display identity. Hashing keeps those public identity fields out of
    /// SQLite's key column. The material layout is persisted and pinned by
    /// `account_scoped_cache_key_derivation_is_pinned`; changing it makes
    /// every existing account catalog cold.
    #[must_use]
    pub fn for_account(provider: &str, descriptor: &CredentialDescriptor) -> Self {
        let identity = descriptor.account_identity.as_ref();
        let (scope_kind, scope) = identity
            .and_then(|identity| identity.account_id.as_deref().map(|id| ("id", id)))
            .or_else(|| {
                identity
                    .and_then(|identity| identity.email.as_deref().map(|email| ("email", email)))
            })
            .unwrap_or(("display", &descriptor.identity));
        let issuer = identity
            .and_then(|identity| identity.issuer.as_deref())
            .unwrap_or("");
        let mut material = format!(
            "{provider}\0{}\0{}\0{issuer}\0{scope_kind}\0{scope}",
            descriptor.alias.as_str(),
            auth_method_tag(descriptor.auth_method),
        );
        if descriptor.auth_method == AuthMethod::ApiKey {
            // API-key validators often return the same display identity for
            // distinct keys. The intake timestamp is retained for a same-key
            // re-login and advanced when a different key replaces it.
            let intake_epoch = identity.map_or(0, |identity| identity.captured_at);
            material.push('\0');
            material.push_str(&intake_epoch.to_string());
        }
        Self(format!(
            "{ACCOUNT_SCOPED_PREFIX}{}",
            blake3::hash(material.as_bytes()).to_hex()
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
