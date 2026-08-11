//! Status-aware provider credential resolution.
//!
//! Owned invariant — ROTATION IS A SEAM, NOT A POLICY: when the active
//! credential is currently limited or expired, this module invokes
//! [`RotationCallback`] exactly once and applies the decision mechanically,
//! with a single hop — an unusable rotation target is reported as an error,
//! never re-delegated. The deciding policy lives outside this crate.

use std::time::{SystemTime, UNIX_EPOCH};

use haider_protocol::credential::{CredentialDescriptor, CredentialStatus, RotationCause};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;

use crate::{AccountStore, AccountsResult, SecretHandle, StoreLike, Vault, accounts_error};

/// The decision returned when an active credential is currently rate-limited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationDecision {
    /// Resolve this alternate alias for the current request.
    RotateTo(CredentialAlias),
    /// Leave selection unchanged and report a retryable limited error.
    Wait,
    /// Leave selection unchanged and report a non-retryable limited error.
    Stop,
}

/// Why credential resolution is asking the one-hop rotation policy.
///
/// Authentication failures are intentionally not represented as invented
/// rate-limit deadlines. Both map to the protocol's honest `Error` cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationTrigger {
    RateLimit { until_ms: u64 },
    AuthExpired,
    RefreshFailed,
}

impl RotationTrigger {
    #[must_use]
    pub fn cause(self) -> RotationCause {
        match self {
            Self::RateLimit { .. } => RotationCause::RateLimit,
            Self::AuthExpired | Self::RefreshFailed => RotationCause::Error,
        }
    }
}

/// Callback seam for the future credential-rotation policy.
///
/// This crate invokes the callback but intentionally supplies no policy.
pub trait RotationCallback: Send + Sync {
    /// Decides what to do about `alias` being rate-limited until `until_ms`.
    fn on_limited(&self, alias: &CredentialAlias, until_ms: u64) -> RotationDecision;

    /// Decides a typed rotation request. Existing limited-only policies keep
    /// their behavior; authentication failures fail closed until explicitly
    /// supported.
    fn on_rotation(&self, alias: &CredentialAlias, trigger: RotationTrigger) -> RotationDecision {
        match trigger {
            RotationTrigger::RateLimit { until_ms } => self.on_limited(alias, until_ms),
            RotationTrigger::AuthExpired | RotationTrigger::RefreshFailed => RotationDecision::Stop,
        }
    }
}

/// Resolves provider descriptors and secret handles.
pub struct Resolver<'a, S, V: ?Sized, R: ?Sized> {
    accounts: &'a AccountStore<S>,
    vault: &'a V,
    rotation: &'a R,
}

impl<'a, S, V, R> Resolver<'a, S, V, R>
where
    S: StoreLike,
    V: Vault + ?Sized,
    R: RotationCallback + ?Sized,
{
    /// Creates a resolver over descriptor, vault, and rotation ports.
    #[must_use]
    pub fn new(accounts: &'a AccountStore<S>, vault: &'a V, rotation: &'a R) -> Self {
        Self {
            accounts,
            vault,
            rotation,
        }
    }

    /// Resolves the active usable credential for `provider`.
    ///
    /// A `Limited` status whose `until_ms` has passed counts as usable and
    /// does not invoke the rotation callback.
    pub fn resolve_for_provider(
        &self,
        provider: &str,
    ) -> AccountsResult<(CredentialDescriptor, SecretHandle)> {
        let descriptor = self.resolve_descriptor_for_provider(provider)?;
        self.resolve_descriptor(descriptor)
    }

    /// Selects the active usable descriptor without resolving secret bytes.
    ///
    /// Daemon account services use this half of the resolver while they own
    /// descriptor state, then release that ownership before awaiting vault or
    /// refresh I/O.
    pub fn resolve_descriptor_for_provider(
        &self,
        provider: &str,
    ) -> AccountsResult<CredentialDescriptor> {
        let now_ms = system_now_ms()?;
        let active = self
            .accounts
            .active_for_provider(provider)
            .cloned()
            .ok_or_else(|| {
                accounts_error(
                    ErrorCode::CredentialMissing,
                    format!("provider `{provider}` has no active credential"),
                    false,
                )
            })?;

        match active.status {
            CredentialStatus::Ok => Ok(active),
            CredentialStatus::Limited { until_ms } if now_ms >= until_ms => Ok(active),
            CredentialStatus::Limited { until_ms } => self.resolve_alternate_descriptor(
                provider,
                &active.alias,
                RotationTrigger::RateLimit { until_ms },
            ),
            CredentialStatus::Expired => self.resolve_alternate_descriptor(
                provider,
                &active.alias,
                RotationTrigger::AuthExpired,
            ),
            CredentialStatus::NeedsAttention { .. } => self.resolve_alternate_descriptor(
                provider,
                &active.alias,
                RotationTrigger::AuthExpired,
            ),
            CredentialStatus::Revoked => Err(unusable(&active.alias, "revoked")),
        }
    }

    /// Applies the rotation callback's decision for a currently-limited active
    /// credential. Single hop: a `RotateTo` target must belong to `provider`
    /// and be usable now, otherwise the resolution fails without another
    /// callback round.
    pub fn resolve_alternate(
        &self,
        provider: &str,
        active_alias: &CredentialAlias,
        trigger: RotationTrigger,
    ) -> AccountsResult<(CredentialDescriptor, SecretHandle)> {
        let descriptor = self.resolve_alternate_descriptor(provider, active_alias, trigger)?;
        self.resolve_descriptor(descriptor)
    }

    /// Applies the one-hop alternate checks without resolving secret bytes.
    ///
    /// This is the single load-bearing helper for every trigger: policy is
    /// invoked once, and the selected target must be same-provider and usable
    /// now. An unusable target is never offered back to policy.
    pub fn resolve_alternate_descriptor(
        &self,
        provider: &str,
        active_alias: &CredentialAlias,
        trigger: RotationTrigger,
    ) -> AccountsResult<CredentialDescriptor> {
        match self.rotation.on_rotation(active_alias, trigger) {
            RotationDecision::RotateTo(alias) => {
                let alternate = self.accounts.get(&alias).cloned().ok_or_else(|| {
                    accounts_error(
                        ErrorCode::CredentialMissing,
                        format!("rotation selected unknown credential alias `{alias}`"),
                        false,
                    )
                })?;
                if alternate.provider != provider {
                    return Err(accounts_error(
                        ErrorCode::InvalidArgument,
                        format!(
                            "rotation selected credential alias `{alias}` for provider `{}`, not `{provider}`",
                            alternate.provider
                        ),
                        false,
                    ));
                }
                match alternate.status {
                    CredentialStatus::Ok => Ok(alternate),
                    CredentialStatus::Limited { until_ms } if system_now_ms()? >= until_ms => {
                        Ok(alternate)
                    }
                    CredentialStatus::Limited { until_ms } => {
                        Err(limited(&alternate.alias, until_ms, true))
                    }
                    CredentialStatus::Expired => Err(unusable(&alternate.alias, "expired")),
                    CredentialStatus::NeedsAttention { .. } => {
                        Err(unusable(&alternate.alias, "needs attention"))
                    }
                    CredentialStatus::Revoked => Err(unusable(&alternate.alias, "revoked")),
                }
            }
            RotationDecision::Wait => Err(trigger_error(active_alias, trigger, true)),
            RotationDecision::Stop => Err(trigger_error(active_alias, trigger, false)),
        }
    }

    fn resolve_descriptor(
        &self,
        descriptor: CredentialDescriptor,
    ) -> AccountsResult<(CredentialDescriptor, SecretHandle)> {
        let secret = self.vault.resolve(&descriptor.alias)?;
        Ok((descriptor, secret))
    }
}

fn trigger_error(
    alias: &CredentialAlias,
    trigger: RotationTrigger,
    retryable: bool,
) -> haider_protocol::error::HaiderError {
    match trigger {
        RotationTrigger::RateLimit { until_ms } => limited(alias, until_ms, retryable),
        RotationTrigger::AuthExpired => accounts_error(
            ErrorCode::Unauthorized,
            format!("credential alias `{alias}` OAuth authorization expired"),
            retryable,
        ),
        RotationTrigger::RefreshFailed => accounts_error(
            ErrorCode::Unauthorized,
            format!("credential alias `{alias}` OAuth refresh failed"),
            retryable,
        ),
    }
}

/// Current Unix time in milliseconds; errors instead of panicking on a
/// pre-epoch or absurdly far-future clock.
fn system_now_ms() -> AccountsResult<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            accounts_error(
                ErrorCode::Internal,
                format!("system clock is before Unix epoch: {error}"),
                false,
            )
        })?;
    u64::try_from(duration.as_millis()).map_err(|_| {
        accounts_error(
            ErrorCode::Internal,
            "current timestamp does not fit u64",
            false,
        )
    })
}

fn limited(
    alias: &CredentialAlias,
    until_ms: u64,
    retryable: bool,
) -> haider_protocol::error::HaiderError {
    accounts_error(
        ErrorCode::CredentialLimited,
        format!("credential alias `{alias}` is limited until epoch-ms {until_ms}"),
        retryable,
    )
}

fn unusable(alias: &CredentialAlias, status: &str) -> haider_protocol::error::HaiderError {
    accounts_error(
        ErrorCode::Unauthorized,
        format!("credential alias `{alias}` is {status}"),
        false,
    )
}
