//! Versioned OAuth token bundles.
//!
//! The encoding is deliberately a compact binary envelope rather than JSON:
//! token bytes are written directly into a zeroizing destination and decoded
//! directly into zeroizing fields. No intermediate `String`, descriptor, or
//! receipt ever contains access, refresh, or ID-token material.

use std::fmt;

use haider_protocol::error::ErrorCode;
use zeroize::Zeroizing;

use crate::{AccountsResult, SecretHandle, accounts_error};

const MAGIC: &[u8] = b"HAIDER_OAUTH_BUNDLE\x01";
const REFRESH_ON_FIRST_USE_MARKER: &[u8] = b"HAIDER_IMPORT_REFRESH\x01";
const LIFECYCLE_EXTENSION_MARKER: &[u8] = b"HAIDER_OAUTH_LIFECYCLE\x01";
const MAX_BUNDLE_BYTES: usize = 256 * 1024;
const MAX_FIELD_BYTES: usize = 64 * 1024;
const MAX_SCOPES: usize = 128;

/// Verified, non-secret identity facts retained after an ID token or
/// authenticated user-info response has been validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthIdentityV1 {
    pub subject_hash: String,
    pub display_identity: String,
}

/// Opaque version-one OAuth credential stored as one vault payload.
///
/// `Debug` never prints either token. ID tokens are intentionally absent:
/// only [`OAuthIdentityV1`] survives verification.
pub struct OAuthTokenBundleV1 {
    pub provider_id: String,
    pub issuer: String,
    pub audience: String,
    pub resource: Option<String>,
    pub token_type: String,
    access_token: Zeroizing<Vec<u8>>,
    refresh_token: Option<Zeroizing<Vec<u8>>>,
    pub expires_at_unix_ms: u64,
    pub refresh_expires_at_unix_ms: Option<u64>,
    pub granted_scopes: Vec<String>,
    pub identity: OAuthIdentityV1,
    pub generation: u64,
    /// Provider-computed turn-boundary threshold for proactive refresh.
    /// `None` preserves the legacy fixed-skew policy.
    pub refresh_after_unix_ms: Option<u64>,
    /// Until this instant the exact stored rotating refresh credential is a
    /// known terminal rejection and must surface re-login without replay.
    pub refresh_rejected_until_unix_ms: Option<u64>,
    refresh_on_first_use: bool,
}

impl fmt::Debug for OAuthTokenBundleV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthTokenBundleV1")
            .field("provider_id", &self.provider_id)
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("resource", &self.resource)
            .field("token_type", &self.token_type)
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at_unix_ms", &self.expires_at_unix_ms)
            .field(
                "refresh_expires_at_unix_ms",
                &self.refresh_expires_at_unix_ms,
            )
            .field("granted_scopes", &self.granted_scopes)
            .field("identity", &self.identity)
            .field("generation", &self.generation)
            .field("refresh_after_unix_ms", &self.refresh_after_unix_ms)
            .field(
                "refresh_rejected_until_unix_ms",
                &self.refresh_rejected_until_unix_ms,
            )
            .finish()
    }
}

impl OAuthTokenBundleV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider_id: String,
        issuer: String,
        audience: String,
        resource: Option<String>,
        token_type: String,
        access_token: Zeroizing<Vec<u8>>,
        refresh_token: Option<Zeroizing<Vec<u8>>>,
        expires_at_unix_ms: u64,
        refresh_expires_at_unix_ms: Option<u64>,
        granted_scopes: Vec<String>,
        identity: OAuthIdentityV1,
        generation: u64,
    ) -> AccountsResult<Self> {
        let bundle = Self {
            provider_id,
            issuer,
            audience,
            resource,
            token_type,
            access_token,
            refresh_token,
            expires_at_unix_ms,
            refresh_expires_at_unix_ms,
            granted_scopes,
            identity,
            generation,
            refresh_after_unix_ms: None,
            refresh_rejected_until_unix_ms: None,
            refresh_on_first_use: false,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    /// Encodes directly into a zeroizing buffer suitable for [`crate::Vault::put`].
    pub fn encode(&self) -> AccountsResult<Zeroizing<Vec<u8>>> {
        self.validate()?;
        let mut out = Zeroizing::new(Vec::with_capacity(
            MAGIC.len()
                + self.provider_id.len()
                + self.issuer.len()
                + self.audience.len()
                + self.resource.as_ref().map_or(0, String::len)
                + self.access_token.len()
                + self.refresh_token.as_ref().map_or(0, |token| token.len())
                + 256,
        ));
        out.extend_from_slice(MAGIC);
        push_u64(&mut out, self.generation);
        push_u64(&mut out, self.expires_at_unix_ms);
        push_optional_u64(&mut out, self.refresh_expires_at_unix_ms);
        push_bytes(&mut out, self.provider_id.as_bytes())?;
        push_bytes(&mut out, self.issuer.as_bytes())?;
        push_bytes(&mut out, self.audience.as_bytes())?;
        match &self.resource {
            Some(resource) => {
                out.push(1);
                push_bytes(&mut out, resource.as_bytes())?;
            }
            None => out.push(0),
        }
        push_bytes(&mut out, self.token_type.as_bytes())?;
        push_bytes(&mut out, self.access_token.as_slice())?;
        match &self.refresh_token {
            Some(token) => {
                out.push(1);
                push_bytes(&mut out, token.as_slice())?;
            }
            None => out.push(0),
        }
        let scope_count = u16::try_from(self.granted_scopes.len())
            .map_err(|_| invalid_bundle("too many OAuth scopes"))?;
        out.extend_from_slice(&scope_count.to_be_bytes());
        for scope in &self.granted_scopes {
            push_bytes(&mut out, scope.as_bytes())?;
        }
        push_bytes(&mut out, self.identity.subject_hash.as_bytes())?;
        push_bytes(&mut out, self.identity.display_identity.as_bytes())?;
        if self.refresh_after_unix_ms.is_some() || self.refresh_rejected_until_unix_ms.is_some() {
            out.extend_from_slice(LIFECYCLE_EXTENSION_MARKER);
            out.push(u8::from(self.refresh_on_first_use));
            push_optional_u64(&mut out, self.refresh_after_unix_ms);
            push_optional_u64(&mut out, self.refresh_rejected_until_unix_ms);
        } else if self.refresh_on_first_use {
            out.extend_from_slice(REFRESH_ON_FIRST_USE_MARKER);
        }
        if out.len() > MAX_BUNDLE_BYTES {
            return Err(invalid_bundle("OAuth token bundle is oversized"));
        }
        Ok(out)
    }

    /// Decodes a vault payload without ever formatting its contents.
    pub fn decode(bytes: &[u8]) -> AccountsResult<Self> {
        if bytes.len() > MAX_BUNDLE_BYTES || !bytes.starts_with(MAGIC) {
            return Err(invalid_bundle("OAuth token bundle version is invalid"));
        }
        let mut reader = Reader::new(&bytes[MAGIC.len()..]);
        let generation = reader.u64()?;
        let expires_at_unix_ms = reader.u64()?;
        let refresh_expires_at_unix_ms = reader.optional_u64()?;
        let provider_id = reader.public_string()?;
        let issuer = reader.public_string()?;
        let audience = reader.public_string()?;
        let resource = match reader.byte()? {
            0 => None,
            1 => Some(reader.public_string()?),
            _ => return Err(invalid_bundle("OAuth resource marker is invalid")),
        };
        let token_type = reader.public_string()?;
        let access_token = reader.secret()?;
        let refresh_token = match reader.byte()? {
            0 => None,
            1 => Some(reader.secret()?),
            _ => return Err(invalid_bundle("OAuth refresh-token marker is invalid")),
        };
        let scope_count = usize::from(reader.u16()?);
        if scope_count > MAX_SCOPES {
            return Err(invalid_bundle("OAuth token bundle has too many scopes"));
        }
        let mut granted_scopes = Vec::with_capacity(scope_count);
        for _ in 0..scope_count {
            granted_scopes.push(reader.public_string()?);
        }
        let subject_hash = reader.public_string()?;
        let display_identity = reader.public_string()?;
        let (refresh_on_first_use, refresh_after_unix_ms, refresh_rejected_until_unix_ms) =
            if reader.is_empty() {
                (false, None, None)
            } else if reader.remaining == REFRESH_ON_FIRST_USE_MARKER {
                (true, None, None)
            } else if reader.remaining.starts_with(LIFECYCLE_EXTENSION_MARKER) {
                reader.take(LIFECYCLE_EXTENSION_MARKER.len())?;
                let refresh_on_first_use = match reader.byte()? {
                    0 => false,
                    1 => true,
                    _ => return Err(invalid_bundle("OAuth lifecycle flags are invalid")),
                };
                let refresh_after_unix_ms = reader.optional_u64()?;
                let refresh_rejected_until_unix_ms = reader.optional_u64()?;
                if !reader.is_empty() {
                    return Err(invalid_bundle("OAuth token bundle has trailing bytes"));
                }
                (
                    refresh_on_first_use,
                    refresh_after_unix_ms,
                    refresh_rejected_until_unix_ms,
                )
            } else {
                return Err(invalid_bundle("OAuth token bundle has trailing bytes"));
            };
        let mut bundle = Self::new(
            provider_id,
            issuer,
            audience,
            resource,
            token_type,
            access_token,
            refresh_token,
            expires_at_unix_ms,
            refresh_expires_at_unix_ms,
            granted_scopes,
            OAuthIdentityV1 {
                subject_hash,
                display_identity,
            },
            generation,
        )?;
        bundle.refresh_on_first_use = refresh_on_first_use;
        bundle.refresh_after_unix_ms = refresh_after_unix_ms;
        bundle.refresh_rejected_until_unix_ms = refresh_rejected_until_unix_ms;
        Ok(bundle)
    }

    /// Produces the only credential bytes adapters may receive for OAuth:
    /// the access token, never the encoded bundle or refresh token.
    #[must_use]
    pub fn access_token_handle(&self) -> SecretHandle {
        SecretHandle::from_zeroizing(&self.access_token)
    }

    #[must_use]
    pub fn refresh_token(&self) -> Option<&[u8]> {
        self.refresh_token.as_ref().map(|token| token.as_slice())
    }

    #[must_use]
    pub fn access_token(&self) -> &[u8] {
        self.access_token.as_slice()
    }

    /// Marks an imported fallback bundle for one eager broker refresh.
    ///
    /// The marker is stored inside the vault payload, never in a descriptor
    /// or receipt. A refreshed bundle is built normally and therefore clears
    /// it durably.
    #[must_use]
    pub fn with_refresh_on_first_use(mut self) -> Self {
        self.refresh_on_first_use = true;
        self
    }

    #[must_use]
    pub fn refresh_on_first_use(&self) -> bool {
        self.refresh_on_first_use
    }

    /// Stores the exact provider-owned proactive refresh boundary.
    #[must_use]
    pub fn with_refresh_after(mut self, refresh_after_unix_ms: u64) -> Self {
        self.refresh_after_unix_ms = Some(refresh_after_unix_ms);
        self
    }

    /// Tombstones the exact stored rotating refresh credential until `until`.
    #[must_use]
    pub fn with_refresh_rejected_until(mut self, until: u64) -> Self {
        self.refresh_rejected_until_unix_ms = Some(until);
        self
    }

    fn validate(&self) -> AccountsResult<()> {
        if self.provider_id.trim().is_empty()
            || self.issuer.trim().is_empty()
            || self.audience.trim().is_empty()
            || self
                .resource
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            || !self.token_type.eq_ignore_ascii_case("bearer")
            || self.access_token.is_empty()
            || self.expires_at_unix_ms == 0
            || self.identity.subject_hash.trim().is_empty()
            || self.identity.display_identity.trim().is_empty()
            || self.granted_scopes.len() > MAX_SCOPES
            || self.granted_scopes.iter().any(|scope| scope.is_empty())
        {
            return Err(invalid_bundle("OAuth token bundle fields are invalid"));
        }
        Ok(())
    }
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_optional_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            out.push(1);
            push_u64(out, value);
        }
        None => out.push(0),
    }
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> AccountsResult<()> {
    if bytes.len() > MAX_FIELD_BYTES {
        return Err(invalid_bundle("OAuth token bundle field is oversized"));
    }
    let length =
        u32::try_from(bytes.len()).map_err(|_| invalid_bundle("OAuth field length overflow"))?;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(remaining: &'a [u8]) -> Self {
        Self { remaining }
    }

    fn take(&mut self, length: usize) -> AccountsResult<&'a [u8]> {
        if length > MAX_FIELD_BYTES || self.remaining.len() < length {
            return Err(invalid_bundle("OAuth token bundle is truncated"));
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    fn byte(&mut self) -> AccountsResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> AccountsResult<u16> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| invalid_bundle("OAuth u16 is truncated"))?;
        Ok(u16::from_be_bytes(bytes))
    }

    fn u32(&mut self) -> AccountsResult<u32> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| invalid_bundle("OAuth u32 is truncated"))?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn u64(&mut self) -> AccountsResult<u64> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| invalid_bundle("OAuth u64 is truncated"))?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn optional_u64(&mut self) -> AccountsResult<Option<u64>> {
        match self.byte()? {
            0 => Ok(None),
            1 => self.u64().map(Some),
            _ => Err(invalid_bundle("OAuth optional-u64 marker is invalid")),
        }
    }

    fn length_prefixed(&mut self) -> AccountsResult<&'a [u8]> {
        let length = usize::try_from(self.u32()?)
            .map_err(|_| invalid_bundle("OAuth field length is invalid"))?;
        self.take(length)
    }

    fn public_string(&mut self) -> AccountsResult<String> {
        String::from_utf8(self.length_prefixed()?.to_vec())
            .map_err(|_| invalid_bundle("OAuth public metadata is not UTF-8"))
    }

    fn secret(&mut self) -> AccountsResult<Zeroizing<Vec<u8>>> {
        Ok(Zeroizing::new(self.length_prefixed()?.to_vec()))
    }

    fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }
}

fn invalid_bundle(message: &'static str) -> haider_protocol::error::HaiderError {
    accounts_error(ErrorCode::Unauthorized, message, false)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn bundle() -> OAuthTokenBundleV1 {
        OAuthTokenBundleV1::new(
            "fake".into(),
            "https://issuer.invalid".into(),
            "fake-resource".into(),
            Some("https://api.invalid".into()),
            "Bearer".into(),
            Zeroizing::new(b"ACCESS_SENTINEL_42".to_vec()),
            Some(Zeroizing::new(b"REFRESH_SENTINEL_42".to_vec())),
            123_456,
            Some(234_567),
            vec!["openid".into(), "inference".into()],
            OAuthIdentityV1 {
                subject_hash: "subject-hash".into(),
                display_identity: "fake@example.invalid".into(),
            },
            7,
        )
        .expect("bundle")
    }

    #[test]
    fn token_bundle_round_trip_is_versioned_and_access_only_handle() {
        let bundle = bundle();
        let encoded = bundle.encode().expect("encode");
        assert!(encoded.starts_with(MAGIC));
        let decoded = OAuthTokenBundleV1::decode(&encoded).expect("decode");
        assert_eq!(decoded.provider_id, "fake");
        assert_eq!(decoded.audience, "fake-resource");
        assert_eq!(decoded.resource.as_deref(), Some("https://api.invalid"));
        assert_eq!(decoded.generation, 7);
        assert_eq!(
            decoded.access_token_handle().expose_secret(),
            b"ACCESS_SENTINEL_42"
        );
        assert_eq!(
            decoded.refresh_token(),
            Some(b"REFRESH_SENTINEL_42".as_slice())
        );
    }

    #[test]
    fn token_bundle_debug_redacts_both_tokens() {
        let debug = format!("{:?}", bundle());
        assert!(!debug.contains("ACCESS_SENTINEL_42"));
        assert!(!debug.contains("REFRESH_SENTINEL_42"));
        assert!(debug.matches("[REDACTED]").count() >= 2);
    }

    #[test]
    fn malformed_and_oversized_bundles_fail_without_echoing_bytes() {
        let sentinel = b"RAW_BUNDLE_ERROR_SENTINEL";
        let error = OAuthTokenBundleV1::decode(sentinel).expect_err("must reject");
        let formatted = format!("{error:?}");
        assert!(!formatted.contains("RAW_BUNDLE_ERROR_SENTINEL"));
        let oversized = vec![0; MAX_BUNDLE_BYTES + 1];
        OAuthTokenBundleV1::decode(&oversized).expect_err("oversized");
    }
}
