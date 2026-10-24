//! Account/credential descriptors (§4.4). The secret NEVER appears in any
//! protocol type — descriptors carry aliases and vault references only.

use crate::ids::CredentialAlias;
use serde::{Deserialize, Serialize};
use unicode_casefold::UnicodeCaseFold as _;
use unicode_normalization::UnicodeNormalization as _;

/// Informational, secret-free account facts captured at credential intake.
///
/// These fields are never authentication authority. In particular,
/// `verified = false` is the only honest value for claims decoded from a JWT
/// without checking its signature against the issuer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    pub captured_at: u64,
    pub verified: bool,
}

impl AccountIdentity {
    /// Returns whether two records identify the same stable provider account.
    ///
    /// A provider account ID is authoritative when both records carry one.
    /// Otherwise a Unicode-folded, NFKC email match supports migration from older
    /// rows that predate a provider's stable-ID capture. Display labels,
    /// subscription plans, and credential bytes are never identity keys.
    #[must_use]
    pub fn same_stable_principal(&self, other: &Self) -> bool {
        match (self.account_id.as_deref(), other.account_id.as_deref()) {
            (Some(left), Some(right)) => left == right,
            _ => self
                .email
                .as_deref()
                .zip(other.email.as_deref())
                .is_some_and(|(left, right)| Self::emails_match(left, right)),
        }
    }

    /// Compare public email claims after Unicode compatibility normalization
    /// and full default case folding. An email remains only a migration hint.
    #[must_use]
    pub fn emails_match(left: &str, right: &str) -> bool {
        let normalize = |email: &str| email.nfkc().case_fold().nfkc().collect::<String>();
        normalize(left) == normalize(right)
    }

    /// Whether `email` names more than one stable account id among
    /// `identities`. An email-only (legacy) record sharing such an email
    /// cannot be bridged to any one of those accounts automatically; it needs
    /// explicit operator review. Callers pass one provider's identities.
    #[must_use]
    pub fn email_names_several_accounts<'a>(
        email: &str,
        identities: impl IntoIterator<Item = &'a AccountIdentity>,
    ) -> bool {
        let mut first: Option<&str> = None;
        for identity in identities {
            let (Some(other_email), Some(account_id)) =
                (identity.email.as_deref(), identity.account_id.as_deref())
            else {
                continue;
            };
            if !Self::emails_match(email, other_email) {
                continue;
            }
            match first {
                None => first = Some(account_id),
                Some(seen) if seen != account_id => return true,
                Some(_) => {}
            }
        }
        false
    }

    /// Whether this is an email-only record whose email matches `email`.
    #[must_use]
    pub fn is_email_only_match(&self, email: &str) -> bool {
        self.account_id.is_none()
            && self
                .email
                .as_deref()
                .is_some_and(|own| Self::emails_match(own, email))
    }

    /// Normalizes one untrusted informational claim before it can reach a
    /// descriptor or operator surface.
    #[must_use]
    pub fn sanitized_field(value: &str) -> Option<String> {
        let value: String = value
            .chars()
            .filter(|character| !character.is_control())
            .take(512)
            .collect();
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }

    /// A concise operator-facing label with no credential material.
    #[must_use]
    pub fn summary(&self) -> String {
        let principal = self
            .email
            .as_deref()
            .or(self.display_name.as_deref())
            .or(self.account_id.as_deref())
            .and_then(Self::sanitized_field)
            .unwrap_or_else(|| "unknown account".to_owned());
        self.plan
            .as_deref()
            .and_then(Self::sanitized_field)
            .map_or(principal.clone(), |plan| format!("{principal} · {plan}"))
    }
}

#[cfg(test)]
mod identity_tests {
    use super::{AccountIdentity, AccountPrincipal};

    #[test]
    fn email_fallback_uses_nfkc_and_full_unicode_case_folding() {
        assert!(AccountIdentity::emails_match(
            "STRASSE@EXAMPLE.TEST",
            "Straße@example.test"
        ));
        assert!(AccountIdentity::emails_match(
            "ＫＥＹ@example.test",
            "key@example.test"
        ));
    }

    #[test]
    fn openai_workspace_id_without_user_sub_never_deduplicates() {
        let mut a = AccountIdentity {
            email: Some("one@example.test".into()),
            display_name: None,
            account_id: Some("shared-workspace".into()),
            plan: None,
            issuer: None,
            captured_at: 1,
            verified: false,
        };
        let mut b = a.clone();
        b.email = Some("two@example.test".into());
        assert!(matches!(
            (
                AccountPrincipal::new("openai-oauth", Some(&a)),
                AccountPrincipal::new("openai-oauth", Some(&b)),
            ),
            (Some(left), Some(right)) if !left.same_account(right)
        ));
        a.account_id = Some("user:3:one;workspace:16:shared-workspace".into());
        b.account_id = Some("user:3:two;workspace:16:shared-workspace".into());
        assert!(matches!(
            (
                AccountPrincipal::new("openai-oauth", Some(&a)),
                AccountPrincipal::new("openai-oauth", Some(&b)),
            ),
            (Some(left), Some(right)) if !left.same_account(right)
        ));
    }

    #[test]
    fn only_stable_user_coordinates_are_provably_different() {
        let identity = |account_id: Option<&str>| AccountIdentity {
            email: Some("member@example.test".into()),
            display_name: None,
            account_id: account_id.map(str::to_owned),
            plan: None,
            issuer: None,
            captured_at: 1,
            verified: false,
        };
        let workspace_only = identity(Some("shared-workspace"));
        let user_a = identity(Some("user:1:a;workspace:16:shared-workspace"));
        let user_b = identity(Some("user:1:b;workspace:16:shared-workspace"));
        let no_id = identity(None);
        let openai = |identity| AccountPrincipal::new("openai-oauth", Some(identity));
        let different = |left, right| {
            openai(left)
                .zip(openai(right))
                .is_some_and(|(left, right)| left.provably_different_account(right))
        };
        assert!(different(&user_a, &user_b));
        assert!(!different(&user_a, &user_a));
        // A legacy workspace-only row might be either member.
        assert!(!different(&user_a, &workspace_only));
        assert!(!different(&user_a, &no_id));
        let anthropic = |identity| AccountPrincipal::new("anthropic-oauth", Some(identity));
        // A synthetic grant-hash id (pre-973, or profile unavailable) is a
        // token lineage, not an account: it never proves a difference.
        let legacy = identity(Some("anthropic:0123456789abcdef"));
        let verified = identity(Some("account=a;organization=o"));
        assert!(matches!(
            (anthropic(&legacy), anthropic(&verified)),
            (Some(left), Some(right))
                if left.identity_unverified()
                    && !right.identity_unverified()
                    && !left.provably_different_account(right)
                    && !right.provably_different_account(left)
                    && !left.same_account(right)
        ));
        let (member_a, member_b) = (
            identity(Some("account=a;organization=o")),
            identity(Some("account=b;organization=o")),
        );
        let (Some(left), Some(right)) = (anthropic(&member_a), anthropic(&member_b)) else {
            unreachable!("identities are present");
        };
        assert!(left.provably_different_account(right));
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CredentialDescriptor {
    pub alias: CredentialAlias,
    pub provider: String,
    /// Optional provider API root for endpoint-addressed adapters.
    ///
    /// Absent for provider-owned endpoints such as Anthropic and OpenAI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub auth_method: AuthMethod,
    /// Human account identity (email/handle) for display.
    pub identity: String,
    pub status: CredentialStatus,
    /// Exactly one credential is active per provider.
    pub active: bool,
    /// Operator-chosen display name (v0.0.938). Purely cosmetic: the `alias`
    /// remains the identity every door addresses, so renaming for display can
    /// never break a reference, a receipt, or a removal-in-flight. Absent
    /// until someone sets one — surfaces fall back to `identity`, then
    /// `alias`. Free-form text, bounded and control-stripped by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Additive v0.0.964 account identity. Older account rows omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_identity: Option<AccountIdentity>,
    /// Epoch milliseconds when Haider first committed this account. Rows
    /// written before v0.0.964 retain `None` rather than inventing a date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
}

impl CredentialDescriptor {
    /// This row's provider-scoped dedupe key, if it captured an identity.
    #[must_use]
    pub fn principal(&self) -> Option<AccountPrincipal<'_>> {
        AccountPrincipal::new(&self.provider, self.account_identity.as_ref())
    }
}

/// One provider-scoped public account identity: the ONLY key OAuth
/// re-login dedupe compares (daemon alias matching, the `haider account list`
/// duplicate hint, and the TUI login notice all go through
/// [`Self::same_account`]).
///
/// It borrows public, secret-free facts only — a provider id and an
/// [`AccountIdentity`] — so no credential byte can reach identity matching.
/// Identities from different providers never match, even with equal ids.
#[derive(Debug, Clone, Copy)]
pub struct AccountPrincipal<'a> {
    provider: &'a str,
    identity: &'a AccountIdentity,
}

impl<'a> AccountPrincipal<'a> {
    /// Returns `None` when the record captured no identity: such rows stay
    /// alias-scoped and are never deduplicated.
    #[must_use]
    pub fn new(provider: &'a str, identity: Option<&'a AccountIdentity>) -> Option<Self> {
        identity.map(|identity| Self { provider, identity })
    }

    /// The provider-scoped stable account coordinate, if this record has
    /// one that names a single user. OpenAI's `chatgpt_account_id` names a
    /// workspace shared by Team/Business members, so only a `user:` coordinate
    /// (ID-token subject plus workspace) qualifies there. Older OpenAI rows
    /// that stored the workspace alone have no stable coordinate until their
    /// user subject is recovered.
    ///
    /// A Claude row whose id is the synthetic `anthropic:<grant hash>`
    /// placeholder (pre-973 rows, and rows saved while the profile endpoint
    /// was unavailable) has no stable coordinate either: that hash names a
    /// token lineage, not an account, so it can never prove two rows differ.
    #[must_use]
    pub fn stable_account_id(self) -> Option<&'a str> {
        let id = self.identity.account_id.as_deref()?;
        if self.identity_unverified() {
            return None;
        }
        (self.provider != OPENAI_OAUTH_PROVIDER || id.starts_with("user:")).then_some(id)
    }

    /// Whether this Claude record still carries the synthetic
    /// `anthropic:<grant hash>` placeholder instead of a profile-verified
    /// `account=…;organization=…` coordinate. Such a row is never
    /// identity-deduplicated; it is upgraded when its own token can reach the
    /// profile endpoint, or re-logged in place through its alias.
    #[must_use]
    pub fn identity_unverified(self) -> bool {
        self.provider == ANTHROPIC_OAUTH_PROVIDER
            && self
                .identity
                .account_id
                .as_deref()
                .is_some_and(|id| id.starts_with(ANTHROPIC_UNVERIFIED_ID_PREFIX))
    }

    /// Same provider and same stable account. OpenAI requires a user-level
    /// coordinate on both sides and never falls back to email; other
    /// providers use [`AccountIdentity::same_stable_principal`]. Anthropic
    /// placeholders on either side never authorize identity deduplication,
    /// including an email bridge to a legacy row.
    #[must_use]
    pub fn same_account(self, other: AccountPrincipal<'_>) -> bool {
        if self.provider != other.provider
            || self.identity_unverified()
            || other.identity_unverified()
        {
            return false;
        }
        if self.provider == OPENAI_OAUTH_PROVIDER {
            return self
                .stable_account_id()
                .zip(other.stable_account_id())
                .is_some_and(|(left, right)| left == right);
        }
        self.identity.same_stable_principal(other.identity)
    }

    /// Both records carry stable coordinates and they name different
    /// accounts. Unlike `!same_account`, a record without a stable
    /// coordinate is never *provably* another account.
    #[must_use]
    pub fn provably_different_account(self, other: AccountPrincipal<'_>) -> bool {
        self.provider != other.provider
            || self
                .stable_account_id()
                .zip(other.stable_account_id())
                .is_some_and(|(left, right)| left != right)
    }
}

/// Provider id whose OAuth account id is a shared workspace, not a user.
const OPENAI_OAUTH_PROVIDER: &str = "openai-oauth";
/// Provider id whose unverified rows carry a synthetic grant-hash id.
const ANTHROPIC_OAUTH_PROVIDER: &str = "anthropic-oauth";
/// Prefix of the synthetic Claude id (see [`AccountPrincipal::identity_unverified`]).
pub const ANTHROPIC_UNVERIFIED_ID_PREFIX: &str = "anthropic:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    ApiKey,
    /// Explicit rename: snake_case would mangle the acronym to "o_auth".
    #[serde(rename = "oauth")]
    OAuth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CredentialStatus {
    Ok,
    /// Rate-limited until the given epoch-ms (rotation ladder input).
    Limited {
        until_ms: u64,
    },
    Expired,
    Revoked,
    /// The cached credential remains durable, but its external owner store
    /// cannot currently be read. The daemon keeps using an unexpired cached
    /// access token and surfaces this state only once that token expires.
    NeedsAttention {
        reason: CredentialAttentionReason,
    },
}

/// Actionable external-owner failures carried by an account row. This is
/// deliberately credential-store vocabulary rather than a free-form error so
/// every client can render an honest recovery action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialAttentionReason {
    KeychainDenied,
    KeychainLocked,
    KeychainMissing,
    KeychainUnavailable,
    /// The explicitly enrolled external credential file/root disappeared.
    SourceGone,
    /// The external source exists but cannot be read safely.
    SourceUnreadable,
    /// The source is present but its origin client must broker access.
    OriginClientRequired,
    /// Direct use is intentionally disabled by provider policy.
    PolicyBlocked,
}

/// Surfaced like a model change in the transcript (§4.4 rotation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RotationEvent {
    pub provider: String,
    pub from: CredentialAlias,
    pub to: CredentialAlias,
    pub cause: RotationCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationCause {
    RateLimit,
    Error,
    Manual,
}
