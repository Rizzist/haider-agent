//! Which account(s) a provider model-catalog refresh may authenticate with,
//! and how an account-specific failure is reported (973-accounts-ux).
//!
//! This is the ONE home of the dead-default fallback rule:
//!
//! * An explicit account alias (a session/run pinned with `--account`) is
//!   honoured exactly — it is the only attempt, healthy or not.
//! * Otherwise the attempts are: the active default when it is usable, else
//!   the first usable account of the provider; then every other usable
//!   account in store order. A dead default is used only when no usable
//!   account exists, solely to surface an actionable error.
//! * "Usable" here means [`CredentialStatus::Ok`] or rate-limited
//!   [`CredentialStatus::Limited`] with the catalog's required auth method
//!   (OAuth catalogs need an OAuth account, API-key catalogs an API key).
//!   Quota does not control model-list access; expired, revoked and
//!   needs-attention (e.g. forbidden) accounts are skipped. The same
//!   predicate decides whether an explicit account's failure leaves another
//!   usable account for the provider.
//!
//! Only the catalog credential falls back. The provider default and any
//! session's pinned account are never changed by this module.
//!
//! Each attempt captures its producing account incarnation and route. The
//! completion and durable cache keep that owner even when a fallback serves it.

use super::*;

/// Whether `descriptor` may serve a fallback catalog refresh for `provider`.
fn is_usable_catalog_account(
    descriptor: &CredentialDescriptor,
    provider: &str,
    expected_auth: AuthMethod,
) -> bool {
    descriptor.provider == provider
        && descriptor.auth_method == expected_auth
        && matches!(
            descriptor.status,
            CredentialStatus::Ok | CredentialStatus::Limited { .. }
        )
}

/// The primary account for a refresh (see the module rule), validated
/// against the provider and the catalog's auth requirement.
fn model_refresh_descriptor<'a>(
    accounts: &'a AccountStore<Box<dyn StoreLike>>,
    provider: &str,
    expected_auth: AuthMethod,
    account_alias: Option<&CredentialAlias>,
) -> Result<&'a CredentialDescriptor, HaiderError> {
    let descriptor = if let Some(alias) = account_alias {
        accounts.get(alias).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::CredentialMissing,
                format!("account `{alias}` does not exist"),
                false,
            )
        })?
    } else {
        accounts
            .active_for_provider(provider)
            .filter(|descriptor| is_usable_catalog_account(descriptor, provider, expected_auth))
            .or_else(|| {
                accounts.list().iter().find(|descriptor| {
                    is_usable_catalog_account(descriptor, provider, expected_auth)
                })
            })
            .or_else(|| accounts.active_for_provider(provider))
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::CredentialMissing,
                    "provider has no active credential",
                    false,
                )
            })?
    };
    if descriptor.provider != provider {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            format!(
                "account `{}` belongs to provider `{}`, not `{provider}`",
                descriptor.alias, descriptor.provider
            ),
            false,
        ));
    }
    if descriptor.auth_method != expected_auth {
        let requirement = if expected_auth == AuthMethod::OAuth {
            "OAuth"
        } else {
            "API-key"
        };
        let message = account_alias.map_or_else(
            || format!("provider model discovery requires an active {requirement} credential"),
            |_| {
                format!(
                    "account `{}` cannot serve the model catalog; {requirement} authentication is required",
                    descriptor.alias
                )
            },
        );
        return Err(HaiderError::new(
            ErrorCode::CredentialMissing,
            message,
            false,
        ));
    }
    Ok(descriptor)
}

/// The ordered accounts a catalog refresh tries until one succeeds or a
/// failure is not account-specific (see the module rule).
pub(super) fn model_refresh_candidates(
    accounts: &AccountStore<Box<dyn StoreLike>>,
    provider: &str,
    expected_auth: AuthMethod,
    account_alias: Option<&CredentialAlias>,
) -> Result<Vec<CredentialDescriptor>, HaiderError> {
    let primary = model_refresh_descriptor(accounts, provider, expected_auth, account_alias)?;
    let mut candidates = vec![primary.clone()];
    if account_alias.is_none() {
        candidates.extend(
            accounts
                .list()
                .iter()
                .filter(|candidate| {
                    candidate.alias != primary.alias
                        && is_usable_catalog_account(candidate, provider, expected_auth)
                })
                .cloned(),
        );
    }
    Ok(candidates)
}

/// Whether a provider has a usable account other than `selected`; an explicit
/// account's credential failure then must not mark the whole provider
/// unavailable.
pub(super) fn provider_has_other_usable_account(
    accounts: &AccountStore<Box<dyn StoreLike>>,
    provider: &str,
    selected: &CredentialAlias,
    requirement: &ProviderAuthRequirementWire,
) -> bool {
    let expected_auth = match requirement {
        ProviderAuthRequirementWire::OAuth => AuthMethod::OAuth,
        ProviderAuthRequirementWire::ApiKey => AuthMethod::ApiKey,
        // A catalog without a credential requirement has no account-specific
        // failure to isolate.
        _ => return false,
    };
    accounts.list().iter().any(|descriptor| {
        descriptor.alias != *selected
            && is_usable_catalog_account(descriptor, provider, expected_auth)
    })
}

pub(super) async fn discover_models_with_account(
    broker: &CredentialBroker,
    descriptor: &CredentialDescriptor,
    model_discoverer: &dyn ProviderModelDiscoverer,
    source: CatalogSource,
    etag: Option<&str>,
    linked_source: Option<&CredentialSourceRecord>,
) -> (ProviderModelsRefreshResult, bool) {
    let expected_access = match linked_source
        .map(|source| linked_catalog_credential(source, descriptor))
        .transpose()
    {
        Ok(expected) => expected,
        Err(error) => return (ProviderModelsRefreshResult::Credential(error), true),
    };
    // The discoverer names where the credential goes: a real endpoint only
    // ever receives a live-issued token; the debug fake data plane is
    // numeric loopback.
    let destination = model_discoverer.destination(&source);
    let resolved = match &destination {
        CatalogDestination::InProcess => broker.resolve_in_process(descriptor).await,
        CatalogDestination::Remote => broker.resolve_remote(descriptor).await,
        #[cfg(debug_assertions)]
        CatalogDestination::Loopback(url) => broker.resolve_to_loopback(descriptor, url).await,
    };
    match resolved {
        Ok(access_token) => {
            if expected_access.is_some_and(|digest| {
                *blake3::hash(access_token.expose_secret()).as_bytes() != digest
            }) {
                return (
                    ProviderModelsRefreshResult::Credential(HaiderError::new(
                        ErrorCode::CredentialMissing,
                        "linked credential changed before catalog discovery",
                        true,
                    )),
                    true,
                );
            }
            let access_token = std::str::from_utf8(access_token.expose_secret()).map_err(|_| {
                let message = if descriptor.auth_method == AuthMethod::OAuth {
                    "OAuth access token is not valid UTF-8"
                } else {
                    "API key is not valid UTF-8"
                };
                HaiderError::new(ErrorCode::CredentialMissing, message, false)
            });
            (
                match access_token {
                    Ok(access_token) => ProviderModelsRefreshResult::Discovery(
                        discover_provider_models_bounded(
                            model_discoverer,
                            source,
                            destination,
                            Some(access_token),
                            etag,
                        )
                        .await,
                    ),
                    Err(error) => ProviderModelsRefreshResult::Credential(error),
                },
                false,
            )
        }
        Err(error) => (
            ProviderModelsRefreshResult::Credential(error),
            linked_source.is_some(),
        ),
    }
}

impl ProviderModelsRefreshResult {
    /// Whether this outcome belongs to the credential that was tried, so
    /// another account of the provider may still succeed.
    pub(super) fn is_account_specific_failure(&self) -> bool {
        match self {
            Self::Discovery(Err(error)) => error.is_account_specific(),
            Self::Credential(_) => true,
            Self::Discovery(Ok(_)) => false,
        }
    }
}

pub(super) fn catalog_auth_failure_message(account_alias: Option<&CredentialAlias>) -> String {
    account_alias.map_or_else(
        || "model catalog authentication failed".to_owned(),
        |alias| {
            format!(
                "account `{alias}` cannot authenticate the model catalog; switch to a live account or re-authenticate it"
            )
        },
    )
}

pub(super) fn catalog_credential_failure_message(
    account_alias: Option<&CredentialAlias>,
    message: &str,
) -> String {
    account_alias.map_or_else(
        || message.to_owned(),
        |alias| format!("account `{alias}` cannot supply a model-catalog credential: {message}"),
    )
}

pub(super) fn catalog_discovery_failure_message(
    account_alias: Option<&CredentialAlias>,
    error: &CatalogError,
) -> String {
    if error.is_account_specific() {
        account_alias.map_or_else(
            || error.to_string(),
            |alias| {
                format!(
                    "account `{alias}` cannot authenticate the model catalog: {error}; switch to a live account or re-authenticate it"
                )
            },
        )
    } else {
        error.to_string()
    }
}
