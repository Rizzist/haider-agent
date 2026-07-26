//! macOS Security.framework-backed credential vault.
//!
//! Upholds the SECRETS NEVER ESCAPE law (see `vault`): error messages carry
//! the operation, the alias, and the OS error text — never secret bytes.
//!
//! Retryability mapping: `resolve`/`list` failures are retryable (denials are
//! commonly transient — locked keychain, pending permission UI) while
//! `put`/`delete` failures are conservatively non-retryable; a missing item is
//! `CredentialMissing` on `resolve` and success on `delete`.

use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;

use crate::{AccountsResult, SecretHandle, Vault, accounts_error};

/// Security.framework service used for every Haider credential item.
pub const KEYCHAIN_SERVICE: &str = "ai.haider.agent";

/// Vault backed by the user's macOS Keychain.
///
/// Keychain calls can display operating-system UI. The real integration test is
/// therefore ignored by default so headless CI never waits for Keychain access.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeychainVault;

impl KeychainVault {
    /// Creates a Keychain-backed vault.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::*;

    /// `errSecItemNotFound` from Security.framework.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;

    impl Vault for KeychainVault {
        fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
            set_generic_password(KEYCHAIN_SERVICE, alias.as_str(), secret)
                .map_err(|error| keychain_error("store", alias, error, false))
        }

        fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
            get_generic_password(KEYCHAIN_SERVICE, alias.as_str())
                .map(SecretHandle::new)
                .map_err(|error| {
                    if error.code() == ERR_SEC_ITEM_NOT_FOUND {
                        accounts_error(
                            ErrorCode::CredentialMissing,
                            format!("no secret is stored for credential alias `{alias}`"),
                            false,
                        )
                    } else {
                        keychain_error("resolve", alias, error, true)
                    }
                })
        }

        fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
            match delete_generic_password(KEYCHAIN_SERVICE, alias.as_str()) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
                Err(error) => Err(keychain_error("delete", alias, error, false)),
            }
        }

        fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
            let result = ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(KEYCHAIN_SERVICE)
                .load_attributes(true)
                .limit(Limit::All)
                .search();

            let items = match result {
                Ok(items) => items,
                Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => return Ok(Vec::new()),
                Err(error) => {
                    return Err(accounts_error(
                        ErrorCode::Internal,
                        format!("could not list secrets in macOS Keychain: {error}"),
                        true,
                    ));
                }
            };

            // Items whose attributes cannot be read are skipped rather than
            // failing the whole listing. `simplify_dict` surfaces
            // kSecAttrAccount under the raw key "acct"; "account" is a
            // defensive fallback for other key spellings.
            let mut aliases = items
                .into_iter()
                .filter_map(|item| item.simplify_dict())
                .filter_map(|attributes| {
                    attributes
                        .get("acct")
                        .or_else(|| attributes.get("account"))
                        .cloned()
                })
                .map(CredentialAlias::new)
                .collect::<Vec<_>>();
            aliases.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            aliases.dedup_by(|left, right| left.as_str() == right.as_str());
            Ok(aliases)
        }
    }

    /// `Internal`-code error naming the failed operation and alias; the OS
    /// error contributes status text only, never secret bytes.
    fn keychain_error(
        operation: &str,
        alias: &CredentialAlias,
        error: security_framework::base::Error,
        retryable: bool,
    ) -> haider_protocol::error::HaiderError {
        accounts_error(
            ErrorCode::Internal,
            format!("could not {operation} credential alias `{alias}` in macOS Keychain: {error}"),
            retryable,
        )
    }
}

#[cfg(not(target_os = "macos"))]
impl Vault for KeychainVault {
    fn put(&self, _alias: &CredentialAlias, _secret: &[u8]) -> AccountsResult<()> {
        Err(unsupported())
    }

    fn resolve(&self, _alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        Err(unsupported())
    }

    fn delete(&self, _alias: &CredentialAlias) -> AccountsResult<()> {
        Err(unsupported())
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        Err(unsupported())
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported() -> haider_protocol::error::HaiderError {
    accounts_error(
        ErrorCode::Internal,
        "KeychainVault requires macOS Security.framework",
        false,
    )
}
