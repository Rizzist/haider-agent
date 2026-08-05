//! One-shot environment-to-vault migration bridge.
//!
//! The environment value IS the secret: it is read once, vaulted, and the
//! local copy scrubbed. No error path may echo the value — in particular,
//! `VarError::NotUnicode` Display-prints the raw value and must never be
//! interpolated into a message.

use std::env;

use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;

use crate::{AccountsResult, Vault, accounts_error};

/// Imports one environment variable into `vault`.
///
/// The alias is deterministic (`<provider>-env`) so callers can persist a
/// matching descriptor. Resolution never consults the environment again.
pub fn import_env(
    vault: &dyn Vault,
    provider: &str,
    env_var: &str,
) -> AccountsResult<CredentialAlias> {
    if provider.is_empty() {
        return Err(accounts_error(
            ErrorCode::InvalidArgument,
            "environment credential provider cannot be empty",
            false,
        ));
    }
    if env_var.is_empty() {
        return Err(accounts_error(
            ErrorCode::InvalidArgument,
            "environment variable name cannot be empty",
            false,
        ));
    }

    let mut secret = env::var(env_var).map(String::into_bytes).map_err(|error| {
        // Describe the failure without the error value: `NotUnicode` carries
        // the raw bytes of the would-be secret.
        let reason = match error {
            env::VarError::NotPresent => "is not set",
            env::VarError::NotUnicode(_) => "is not valid UTF-8",
        };
        accounts_error(
            ErrorCode::CredentialMissing,
            format!("environment variable `{env_var}` {reason}"),
            false,
        )
    })?;
    let alias = CredentialAlias::new(format!("{provider}-env"));
    // Scrub the local copy (best-effort) before propagating any put error.
    let result = vault.put(&alias, &secret);
    secret.fill(0);
    result?;
    Ok(alias)
}
