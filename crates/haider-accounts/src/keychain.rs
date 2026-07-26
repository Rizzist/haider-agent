//! macOS Security.framework-backed credential vault.
//!
//! Upholds the SECRETS NEVER ESCAPE law (see `vault`): error messages carry
//! the operation, the alias, and the OS error text — never secret bytes.
//!
//! Retryability is derived from Security.framework's OSStatus: I/O and
//! unavailable/locked/authentication-UI failures are transient, while
//! malformed requests, cancellation, missing entitlements, and unknown
//! statuses are permanent. A missing item is `CredentialMissing` on `resolve`
//! and success on `delete`.

use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;

use crate::{AccountsResult, SecretHandle, Vault, accounts_error};

/// Security.framework service used for every Haider credential item.
pub const KEYCHAIN_SERVICE: &str = "ai.haider.agent";

// Security.framework OSStatus values from SecBase.h. These stay local rather
// than adding a direct security-framework-sys dependency solely for constants.
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_IO: i32 = -36;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_PARAM: i32 = -50;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_USER_CANCELED: i32 = -128;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_MISSING_ENTITLEMENT: i32 = -34_018;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_NOT_AVAILABLE: i32 = -25_291;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_AUTH_FAILED: i32 = -25_293;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25_308;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_INTERACTION_REQUIRED: i32 = -25_315;
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_IN_DARK_WAKE: i32 = -25_320;

/// Maps a Security.framework OSStatus to Haider's stable code and retry flag.
///
/// Unknown statuses are non-retryable: callers should retry only failures
/// explicitly known to clear after I/O recovery, Keychain availability, or
/// authentication UI becomes available.
#[cfg(any(target_os = "macos", test))]
pub(crate) const fn classify_os_status(status: i32) -> (ErrorCode, bool) {
    match status {
        ERR_SEC_ITEM_NOT_FOUND => (ErrorCode::CredentialMissing, false),
        ERR_SEC_IO
        | ERR_SEC_NOT_AVAILABLE
        | ERR_SEC_AUTH_FAILED
        | ERR_SEC_INTERACTION_NOT_ALLOWED
        | ERR_SEC_INTERACTION_REQUIRED
        | ERR_SEC_IN_DARK_WAKE => (ErrorCode::Internal, true),
        ERR_SEC_PARAM | ERR_SEC_USER_CANCELED | ERR_SEC_MISSING_ENTITLEMENT => {
            (ErrorCode::Internal, false)
        }
        _ => (ErrorCode::Internal, false),
    }
}

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

    impl Vault for KeychainVault {
        fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
            set_generic_password(KEYCHAIN_SERVICE, alias.as_str(), secret)
                .map_err(|error| keychain_error("store", alias, error))
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
                        keychain_error("resolve", alias, error)
                    }
                })
        }

        fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
            match delete_generic_password(KEYCHAIN_SERVICE, alias.as_str()) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
                Err(error) => Err(keychain_error("delete", alias, error)),
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
                    let (code, retryable) = classify_os_status(error.code());
                    return Err(accounts_error(
                        code,
                        format!("could not list secrets in macOS Keychain: {error}"),
                        retryable,
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

    /// Classified error naming the failed operation and alias; the OS error
    /// contributes status text only, never secret bytes.
    fn keychain_error(
        operation: &str,
        alias: &CredentialAlias,
        error: security_framework::base::Error,
    ) -> haider_protocol::error::HaiderError {
        let (code, retryable) = classify_os_status(error.code());
        accounts_error(
            code,
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
