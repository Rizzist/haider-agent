//! Credential descriptors, secret vaults, and provider credential resolution.
//!
//! Three owned invariants, each enforced by the named module:
//!
//! - SECRETS NEVER ESCAPE (`vault`): secret bytes cross this crate's public
//!   boundary solely through [`SecretHandle`], whose `Debug`/`Display` are
//!   always redacted. Descriptors and error messages carry aliases and display
//!   metadata only.
//! - ONE ACTIVE PER PROVIDER (`store`): every provider with at least one
//!   descriptor has exactly one active descriptor, revalidated on every load
//!   and commit.
//! - ROTATION IS A SEAM, NOT A POLICY (`resolver`): a currently-limited active
//!   credential is delegated to [`RotationCallback`]; this crate applies the
//!   decision mechanically and ships no policy (that lands in D3b).

mod env_bridge;
mod file_vault;
#[cfg(test)]
mod file_vault_tests;
mod keychain;
#[cfg(test)]
mod keychain_tests;
mod oauth;
mod resolver;
mod store;
mod vault;

pub use env_bridge::import_env;
pub use file_vault::FileVault;
pub use haider_protocol::credential::{
    AuthMethod, CredentialDescriptor, CredentialStatus, RotationEvent,
};
pub use haider_protocol::error::{ErrorCode, HaiderError};
pub use haider_protocol::ids::CredentialAlias;
pub use keychain::{KEYCHAIN_SERVICE, KeychainVault};
pub use oauth::{OAuthIdentityV1, OAuthTokenBundleV1};
pub use resolver::{Resolver, RotationCallback, RotationDecision, RotationTrigger};
pub use store::{ACCOUNTS_FILE_NAME, AccountStore, JsonFileStore, StoreLike};
pub use vault::{MemoryVault, SecretHandle, Vault, VaultRefreshLock};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-accounts";

/// Result type used by account, vault, and resolver operations.
pub type AccountsResult<T> = Result<T, HaiderError>;

/// Builds this crate's [`HaiderError`] (never a `details` payload, and — per
/// the redaction law — never secret bytes in `message`).
pub(crate) fn accounts_error(
    code: ErrorCode,
    message: impl Into<String>,
    retryable: bool,
) -> HaiderError {
    HaiderError::new(code, message, retryable)
}
