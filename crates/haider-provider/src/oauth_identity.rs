//! Provider-owned extraction of informational identity from OAuth material.
//!
//! This is deliberately separate from OAuth authorization. Decoding a JWT
//! payload here does not verify its signature and therefore always yields an
//! identity with `verified = false`. A capture path that independently
//! verifies the issuer response may promote that bit after comparing the
//! facts it verified.

use std::fmt;

use base64::Engine as _;
use haider_protocol::credential::AccountIdentity;
use serde::Deserialize;

use crate::{
    ANTHROPIC_OAUTH_PROVIDER_NAME, GROK_OAUTH_PROVIDER_NAME, KIMI_OAUTH_PROVIDER_NAME,
    OPENAI_OAUTH_PROVIDER_NAME,
};

const MAX_ID_TOKEN_BYTES: usize = 64 * 1024;
const MAX_CLAIMS_BYTES: usize = 32 * 1024;

/// Borrowed OAuth token material. Debug output exposes presence only.
pub struct OAuthTokens<'a> {
    pub access_token: &'a [u8],
    pub refresh_token: Option<&'a [u8]>,
    pub id_token: Option<&'a [u8]>,
    pub captured_at: u64,
}

impl fmt::Debug for OAuthTokens<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthTokens")
            .field("has_access_token", &!self.access_token.is_empty())
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("has_id_token", &self.id_token.is_some())
            .field("captured_at", &self.captured_at)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    TokenTooLarge,
    JwtMalformed,
    ClaimsMalformed,
}

/// Optional authenticated identity lookup metadata. Fetching is owned by the
/// OAuth completion path and happens at capture time only; the fetched
/// document is parsed by [`OAuthIdentitySource::identity_from_endpoint_document`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityEndpoint {
    pub url: &'static str,
    /// Request headers the endpoint requires besides the bearer token.
    pub headers: &'static [(&'static str, &'static str)],
}

/// Claude's authenticated OAuth profile document: the only place a Claude
/// subscription credential exposes a stable account coordinate (its token
/// response carries no ID token or account id).
///
/// Provenance: the endpoint the first-party Claude Code client calls after
/// OAuth sign-in, covered by the `user:profile` scope Haider already requests
/// (Claude Code parity, W5g-7). It requires the OAuth beta header
/// ([`crate::ANTHROPIC_OAUTH_BETA_HEADER`]: [`crate::ANTHROPIC_OAUTH_BETA_VALUE`]).
/// Response shape used here: `{"account": {"uuid", "email", "display_name"?,
/// "full_name"?}, "organization": {"uuid", "organization_type"?}}` — pinned by
/// the daemon's fake `/profile` server test.
pub const ANTHROPIC_OAUTH_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

const ANTHROPIC_OAUTH_PROFILE_HEADERS: &[(&str, &str)] = &[(
    crate::ANTHROPIC_OAUTH_BETA_HEADER,
    crate::ANTHROPIC_OAUTH_BETA_VALUE,
)];

/// One provider-generic identity seam for every OAuth adapter.
pub trait OAuthIdentitySource: Send + Sync {
    fn identity_from_tokens(
        &self,
        tokens: &OAuthTokens<'_>,
    ) -> Result<Option<AccountIdentity>, IdentityError>;

    fn identity_endpoint(&self) -> Option<IdentityEndpoint> {
        None
    }

    /// Parses the document fetched from [`Self::identity_endpoint`]. Like
    /// token decoding, the result is informational (`verified = false`,
    /// `issuer = None`); the fetching owner may promote `verified` because it
    /// authenticated the request itself, and records the issuer it trusts.
    fn identity_from_endpoint_document(
        &self,
        _document: &[u8],
        _captured_at: u64,
    ) -> Result<Option<AccountIdentity>, IdentityError> {
        Ok(None)
    }

    /// Documents why a registered OAuth adapter intentionally has no local
    /// token-derived identity.
    fn unavailable_reason(&self) -> Option<&'static str> {
        None
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OpenAiOAuthIdentitySource;

#[derive(Debug, Clone, Copy)]
pub struct AnthropicOAuthIdentitySource;

#[derive(Debug, Clone, Copy)]
pub struct KimiOAuthIdentitySource;

#[derive(Debug, Clone, Copy)]
pub struct GrokOAuthIdentitySource;

#[derive(Default, Deserialize)]
struct StandardClaims {
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaims>,
}

#[derive(Default, Deserialize)]
struct OpenAiAuthClaims {
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

impl OAuthIdentitySource for OpenAiOAuthIdentitySource {
    fn identity_from_tokens(
        &self,
        tokens: &OAuthTokens<'_>,
    ) -> Result<Option<AccountIdentity>, IdentityError> {
        let Some(claims) = decode_claims(tokens.id_token)? else {
            return Ok(None);
        };
        let OpenAiAuthClaims {
            chatgpt_plan_type,
            chatgpt_account_id,
        } = claims.openai_auth.unwrap_or_default();
        let workspace =
            nonempty(chatgpt_account_id).or_else(|| nonempty(claims.chatgpt_account_id));
        let user = nonempty(claims.sub);
        Ok(Some(AccountIdentity {
            email: nonempty(claims.email),
            display_name: nonempty(claims.name),
            // chatgpt_account_id names a workspace shared by Team/Business
            // members. Never use it without the ID-token's user subject.
            account_id: user
                .map(|user| openai_user_account_coordinate(&user, workspace.as_deref())),
            plan: nonempty(chatgpt_plan_type).or_else(|| nonempty(claims.plan)),
            issuer: nonempty(claims.iss).or_else(|| Some("https://auth.openai.com".to_owned())),
            captured_at: tokens.captured_at,
            verified: false,
        }))
    }
}

impl OAuthIdentitySource for GrokOAuthIdentitySource {
    fn identity_from_tokens(
        &self,
        tokens: &OAuthTokens<'_>,
    ) -> Result<Option<AccountIdentity>, IdentityError> {
        let Some(claims) = decode_claims(tokens.id_token)? else {
            return Ok(None);
        };
        Ok(Some(AccountIdentity {
            email: nonempty(claims.email),
            display_name: nonempty(claims.name),
            account_id: nonempty(claims.sub),
            plan: nonempty(claims.plan),
            issuer: nonempty(claims.iss).or_else(|| Some("https://auth.x.ai".to_owned())),
            captured_at: tokens.captured_at,
            verified: false,
        }))
    }
}

impl OAuthIdentitySource for AnthropicOAuthIdentitySource {
    fn identity_from_tokens(
        &self,
        _tokens: &OAuthTokens<'_>,
    ) -> Result<Option<AccountIdentity>, IdentityError> {
        // Claude's current token response and documented local credential
        // shape contain subscription type but no ID token or stable account
        // identifier. The capture owner may add server-verified facts if the
        // provider exposes them; local decoding cannot.
        Ok(None)
    }

    fn identity_endpoint(&self) -> Option<IdentityEndpoint> {
        Some(IdentityEndpoint {
            url: ANTHROPIC_OAUTH_PROFILE_URL,
            headers: ANTHROPIC_OAUTH_PROFILE_HEADERS,
        })
    }

    fn identity_from_endpoint_document(
        &self,
        document: &[u8],
        captured_at: u64,
    ) -> Result<Option<AccountIdentity>, IdentityError> {
        #[derive(Deserialize)]
        struct Profile {
            account: ProfileAccount,
            organization: ProfileOrganization,
        }
        #[derive(Deserialize)]
        struct ProfileAccount {
            uuid: String,
            // Informational only: the dedupe key is account + organization
            // uuid, so an account without an email still identifies.
            #[serde(default)]
            email: Option<String>,
            #[serde(default)]
            display_name: Option<String>,
            #[serde(default)]
            full_name: Option<String>,
        }
        #[derive(Deserialize)]
        struct ProfileOrganization {
            uuid: String,
            #[serde(default)]
            organization_type: Option<String>,
        }

        let profile = serde_json::from_slice::<Profile>(document)
            .map_err(|_| IdentityError::ClaimsMalformed)?;
        let required = |value: &str| {
            AccountIdentity::sanitized_field(value).ok_or(IdentityError::ClaimsMalformed)
        };
        let account_id = required(&profile.account.uuid)?;
        let organization_id = required(&profile.organization.uuid)?;
        let optional = |value: Option<&str>| value.and_then(AccountIdentity::sanitized_field);
        Ok(Some(AccountIdentity {
            email: optional(profile.account.email.as_deref()),
            display_name: optional(profile.account.display_name.as_deref())
                .or_else(|| optional(profile.account.full_name.as_deref())),
            account_id: Some(anthropic_stable_account_coordinate(
                &account_id,
                &organization_id,
            )),
            plan: optional(profile.organization.organization_type.as_deref()),
            issuer: None,
            captured_at,
            verified: false,
        }))
    }
}

/// A Claude subscription credential is scoped by both the user account and
/// the selected organization. The pair is kept as ONE public stable
/// coordinate in today's `AccountIdentity::account_id` so separate
/// organizations cannot be collapsed merely because they share an owner.
fn anthropic_stable_account_coordinate(account_id: &str, organization_id: &str) -> String {
    format!("account={account_id};organization={organization_id}")
}

/// Length-prefixes both components so distinct claims cannot serialize to
/// the same provider-scoped dedupe coordinate.
pub fn openai_user_account_coordinate(user: &str, workspace: Option<&str>) -> String {
    let workspace = workspace.unwrap_or("");
    format!(
        "user:{}:{user};workspace:{}:{workspace}",
        user.len(),
        workspace.len()
    )
}

/// Extract the workspace for APIs that still require its raw identifier.
/// Old descriptors containing only a workspace id remain readable there,
/// even though they are no longer accepted as a user dedupe key.
pub fn openai_workspace_from_account_coordinate(coordinate: &str) -> Option<&str> {
    let Some(rest) = coordinate.strip_prefix("user:") else {
        return (!coordinate.is_empty()).then_some(coordinate);
    };
    let (length, rest) = rest.split_once(':')?;
    let length = length.parse::<usize>().ok()?;
    let suffix = rest.get(length..)?.strip_prefix(";workspace:")?;
    let (workspace_length, workspace) = suffix.split_once(':')?;
    let workspace_length = workspace_length.parse::<usize>().ok()?;
    (workspace_length == workspace.len() && !workspace.is_empty()).then_some(workspace)
}

impl OAuthIdentitySource for KimiOAuthIdentitySource {
    fn identity_from_tokens(
        &self,
        _tokens: &OAuthTokens<'_>,
    ) -> Result<Option<AccountIdentity>, IdentityError> {
        Ok(None)
    }

    fn unavailable_reason(&self) -> Option<&'static str> {
        Some("Kimi Code device OAuth returns no ID token or profile identity")
    }
}

static OPENAI: OpenAiOAuthIdentitySource = OpenAiOAuthIdentitySource;
static ANTHROPIC: AnthropicOAuthIdentitySource = AnthropicOAuthIdentitySource;
static KIMI: KimiOAuthIdentitySource = KimiOAuthIdentitySource;
static GROK: GrokOAuthIdentitySource = GrokOAuthIdentitySource;

/// Returns the identity adapter for every release-registered OAuth provider.
#[must_use]
pub fn oauth_identity_source(provider: &str) -> Option<&'static dyn OAuthIdentitySource> {
    match provider {
        OPENAI_OAUTH_PROVIDER_NAME => Some(&OPENAI),
        ANTHROPIC_OAUTH_PROVIDER_NAME => Some(&ANTHROPIC),
        KIMI_OAUTH_PROVIDER_NAME => Some(&KIMI),
        GROK_OAUTH_PROVIDER_NAME => Some(&GROK),
        _ => None,
    }
}

fn decode_claims(id_token: Option<&[u8]>) -> Result<Option<StandardClaims>, IdentityError> {
    let Some(token) = id_token else {
        return Ok(None);
    };
    if token.len() > MAX_ID_TOKEN_BYTES {
        return Err(IdentityError::TokenTooLarge);
    }
    let token = std::str::from_utf8(token).map_err(|_| IdentityError::JwtMalformed)?;
    let mut segments = token.split('.');
    let _header = segments.next().ok_or(IdentityError::JwtMalformed)?;
    let payload = segments.next().ok_or(IdentityError::JwtMalformed)?;
    let _signature = segments.next().ok_or(IdentityError::JwtMalformed)?;
    if segments.next().is_some() || payload.is_empty() {
        return Err(IdentityError::JwtMalformed);
    }
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .map_err(|_| IdentityError::JwtMalformed)?;
    if claims.len() > MAX_CLAIMS_BYTES {
        return Err(IdentityError::TokenTooLarge);
    }
    serde_json::from_slice(&claims)
        .map(Some)
        .map_err(|_| IdentityError::ClaimsMalformed)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| AccountIdentity::sanitized_field(&value))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn openai_nested_claims_are_informational_and_unverified() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            br#"{"sub":"user-1","email":"owner@example.test","https://api.openai.com/auth":{"chatgpt_plan_type":"pro","chatgpt_account_id":"acct-1"}}"#,
        );
        let token = format!("e30.{payload}.signature");
        let tokens = OAuthTokens {
            access_token: b"present",
            refresh_token: None,
            id_token: Some(token.as_bytes()),
            captured_at: 964,
        };
        let identity = OpenAiOAuthIdentitySource
            .identity_from_tokens(&tokens)
            .expect("claims decode")
            .expect("identity present");
        assert_eq!(identity.email.as_deref(), Some("owner@example.test"));
        assert_eq!(identity.plan.as_deref(), Some("pro"));
        assert_eq!(
            identity.account_id.as_deref(),
            Some("user:6:user-1;workspace:6:acct-1")
        );
        assert!(!identity.verified);
    }

    #[test]
    fn anthropic_profile_email_is_optional_but_uuids_are_required() {
        let parse = |document: &str| {
            AnthropicOAuthIdentitySource.identity_from_endpoint_document(document.as_bytes(), 7)
        };
        let identity = parse(
            r#"{"account":{"uuid":"acct-1"},"organization":{"uuid":"org-1","organization_type":"claude_max"}}"#,
        )
        .expect("email-less profile parses")
        .expect("identity");
        assert_eq!(identity.email, None);
        assert_eq!(
            identity.account_id.as_deref(),
            Some("account=acct-1;organization=org-1")
        );
        assert_eq!(
            identity.summary(),
            "account=acct-1;organization=org-1 · claude_max"
        );
        let blank_email =
            parse(r#"{"account":{"uuid":"acct-1","email":"  "},"organization":{"uuid":"org-1"}}"#)
                .expect("blank email parses")
                .expect("identity");
        assert_eq!(blank_email.email, None);
        assert!(
            parse(r#"{"account":{"email":"a@example.test"},"organization":{"uuid":"o"}}"#).is_err()
        );
        assert!(parse(r#"{"account":{"uuid":"a"},"organization":{"uuid":" "}}"#).is_err());
    }

    #[test]
    fn openai_workspace_members_have_distinct_principals() {
        let identity = |sub: &str| {
            let claims = format!(
                r#"{{"sub":"{sub}","email":"{sub}@example.test","https://api.openai.com/auth":{{"chatgpt_account_id":"shared-workspace"}}}}"#
            );
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
            let token = format!("e30.{payload}.signature");
            OpenAiOAuthIdentitySource
                .identity_from_tokens(&OAuthTokens {
                    access_token: b"synthetic",
                    refresh_token: None,
                    id_token: Some(token.as_bytes()),
                    captured_at: 1,
                })
                .expect("claims decode")
                .expect("identity")
        };
        let a = identity("user-a");
        let b = identity("user-b");
        assert_ne!(a.account_id, b.account_id);
        assert!(!a.same_stable_principal(&b));
    }

    #[test]
    fn openai_sub_without_workspace_is_still_a_user_coordinate() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"only-user"}"#);
        let token = format!("e30.{payload}.signature");
        let identity = OpenAiOAuthIdentitySource
            .identity_from_tokens(&OAuthTokens {
                access_token: b"synthetic",
                refresh_token: None,
                id_token: Some(token.as_bytes()),
                captured_at: 1,
            })
            .expect("claims decode")
            .expect("identity");
        assert_eq!(
            identity.account_id.as_deref(),
            Some("user:9:only-user;workspace:0:")
        );
    }

    #[test]
    fn openai_workspace_header_uses_raw_workspace_not_user_coordinate() {
        let coordinate = openai_user_account_coordinate("member", Some("workspace"));
        assert_eq!(
            openai_workspace_from_account_coordinate(&coordinate),
            Some("workspace")
        );
        assert_eq!(
            openai_workspace_from_account_coordinate("old-workspace"),
            Some("old-workspace")
        );
        assert_eq!(
            openai_workspace_from_account_coordinate(&openai_user_account_coordinate(
                "member", None
            )),
            None
        );
    }

    #[test]
    fn openai_coordinate_distinguishes_workspaces_and_delimiters() {
        assert_ne!(
            openai_user_account_coordinate("same-user", Some("personal")),
            openai_user_account_coordinate("same-user", Some("team")),
        );
        assert_ne!(
            openai_user_account_coordinate("a;workspace:b", None),
            openai_user_account_coordinate("a", Some("b")),
        );
    }

    #[test]
    fn every_registered_oauth_provider_has_an_explicit_identity_source() {
        for provider in [
            OPENAI_OAUTH_PROVIDER_NAME,
            ANTHROPIC_OAUTH_PROVIDER_NAME,
            KIMI_OAUTH_PROVIDER_NAME,
            GROK_OAUTH_PROVIDER_NAME,
        ] {
            assert!(
                oauth_identity_source(provider).is_some(),
                "missing identity source for {provider}"
            );
        }
        let anthropic = oauth_identity_source(ANTHROPIC_OAUTH_PROVIDER_NAME)
            .expect("Anthropic identity source");
        assert_eq!(
            anthropic.identity_endpoint(),
            Some(IdentityEndpoint {
                url: "https://api.anthropic.com/api/oauth/profile",
                headers: &[("anthropic-beta", "oauth-2025-04-20")],
            })
        );
        assert_eq!(anthropic.unavailable_reason(), None);
        assert!(
            oauth_identity_source(KIMI_OAUTH_PROVIDER_NAME)
                .and_then(|source| source.unavailable_reason())
                .is_some()
        );
    }

    #[test]
    fn oauth_token_debug_reports_presence_only() {
        let tokens = OAuthTokens {
            access_token: b"ACCESS_IDENTITY_SENTINEL",
            refresh_token: Some(b"REFRESH_IDENTITY_SENTINEL"),
            id_token: Some(b"ID_IDENTITY_SENTINEL"),
            captured_at: 964,
        };
        let debug = format!("{tokens:?}");
        for secret in [
            "ACCESS_IDENTITY_SENTINEL",
            "REFRESH_IDENTITY_SENTINEL",
            "ID_IDENTITY_SENTINEL",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(debug.contains("has_id_token: true"));
    }

    #[test]
    fn public_claim_fields_are_control_stripped_and_bounded() {
        let raw = format!("\u{1b}[31m{}", "x".repeat(600));
        let value = AccountIdentity::sanitized_field(&raw).expect("sanitized field");
        assert!(!value.chars().any(char::is_control));
        assert!(value.chars().count() <= 512);
    }
}
