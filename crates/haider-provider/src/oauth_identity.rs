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
/// OAuth completion path and happens at capture time only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityEndpoint {
    pub url: &'static str,
}

/// One provider-generic identity seam for every OAuth adapter.
pub trait OAuthIdentitySource: Send + Sync {
    fn identity_from_tokens(
        &self,
        tokens: &OAuthTokens<'_>,
    ) -> Result<Option<AccountIdentity>, IdentityError>;

    fn identity_endpoint(&self) -> Option<IdentityEndpoint> {
        None
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
        Ok(Some(AccountIdentity {
            email: nonempty(claims.email),
            display_name: nonempty(claims.name),
            account_id: nonempty(chatgpt_account_id)
                .or_else(|| nonempty(claims.chatgpt_account_id)),
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

    fn unavailable_reason(&self) -> Option<&'static str> {
        Some("Claude OAuth currently returns no ID token or stable account identifier")
    }
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
            br#"{"email":"owner@example.test","https://api.openai.com/auth":{"chatgpt_plan_type":"pro","chatgpt_account_id":"acct-1"}}"#,
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
        assert_eq!(identity.account_id.as_deref(), Some("acct-1"));
        assert!(!identity.verified);
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
        assert!(
            oauth_identity_source(ANTHROPIC_OAUTH_PROVIDER_NAME)
                .and_then(|source| source.unavailable_reason())
                .is_some()
        );
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
