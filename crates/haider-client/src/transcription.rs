//! Thin typed helpers for the T1 transcription-secret RPCs
//! (`transcription.secret_get` / `transcription.secret_set`).
//!
//! T1 shipped the wire shapes and the daemon vault; this module is the
//! client seam T2 adds: pure request BUILDERS and response PARSERS that
//! CONSUME `haider-rpc`'s frozen bodies (never redefine them), plus async
//! conveniences over [`RpcClient`]. The TUI's link routes its
//! `LiveCommand`s through the same builders/parsers, so there is exactly
//! one authority for what these requests and replies look like.
//!
//! Secret hygiene: the key travels only as [`SecretWire`] (redacted
//! `Debug`, zeroize-on-drop); the parse never converts a body through a
//! loggable `serde_json::Value`; error text never contains key material.

use haider_rpc::{RequestBody, ResponseBody, SecretWire};

use crate::client::{ClientError, RpcClient};

/// A typed failure from either transcription-secret RPC.
#[derive(Debug)]
pub enum TranscriptionSecretError {
    /// Transport/correlation failure from the client.
    Client(ClientError),
    /// The daemon answered a typed refusal (`code`, `message`) — e.g. the
    /// surface gate (not same-UID UDS / no vault), or key hygiene.
    Refused { code: String, message: String },
    /// The daemon answered with a body this method does not produce — a
    /// skewed daemon; carried honestly, never silently coerced.
    UnexpectedBody,
}

impl std::fmt::Display for TranscriptionSecretError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Refused { code, message } => {
                if message.is_empty() {
                    write!(formatter, "daemon refused ({code})")
                } else {
                    write!(formatter, "daemon refused ({code}): {message}")
                }
            }
            Self::UnexpectedBody => formatter.write_str("daemon answered with an unexpected body"),
        }
    }
}

impl std::error::Error for TranscriptionSecretError {}

/// The `transcription.secret_get` request body (unit — reads carry
/// nothing).
#[must_use]
pub fn secret_get_request() -> RequestBody {
    RequestBody::TranscriptionSecretGet
}

/// The `transcription.secret_set` request body. `clear: true` must ride an
/// EMPTY secret (daemon law); the builder does not pre-judge — the daemon
/// is the authority and refusals come back typed.
#[must_use]
pub fn secret_set_request(secret: SecretWire, clear: bool) -> RequestBody {
    RequestBody::TranscriptionSecretSet { secret, clear }
}

/// Parse a `transcription.secret_get` response: `Ok(Some(secret))` when a
/// key is vaulted, `Ok(None)` when none is (the absent field stays OFF the
/// wire — T1 golden law).
pub fn secret_from_get_response(
    body: ResponseBody,
) -> Result<Option<SecretWire>, TranscriptionSecretError> {
    match body {
        ResponseBody::TranscriptionSecretGet { secret } => Ok(secret),
        ResponseBody::Error { code, message, .. } => {
            Err(TranscriptionSecretError::Refused { code, message })
        }
        _ => Err(TranscriptionSecretError::UnexpectedBody),
    }
}

/// Parse a `transcription.secret_set` response: whether a secret is
/// present AFTER the operation (`true` after a set, `false` after a
/// clear).
pub fn present_from_set_response(body: ResponseBody) -> Result<bool, TranscriptionSecretError> {
    match body {
        ResponseBody::TranscriptionSecretSet { present } => Ok(present),
        ResponseBody::Error { code, message, .. } => {
            Err(TranscriptionSecretError::Refused { code, message })
        }
        _ => Err(TranscriptionSecretError::UnexpectedBody),
    }
}

/// Read the profile's vaulted transcription secret (the Deepgram API key).
/// UDS-only on the daemon side; callers copy into their own zeroizing
/// storage and drop the wire frame promptly.
pub async fn secret_get(
    client: &RpcClient,
) -> Result<Option<SecretWire>, TranscriptionSecretError> {
    let body = client
        .request(secret_get_request())
        .await
        .map_err(TranscriptionSecretError::Client)?;
    secret_from_get_response(body)
}

/// Store (or, with `clear: true` and an empty secret, delete) the profile's
/// transcription secret in the daemon vault. Returns whether a secret is
/// present afterwards.
pub async fn secret_set(
    client: &RpcClient,
    secret: SecretWire,
    clear: bool,
) -> Result<bool, TranscriptionSecretError> {
    let body = client
        .request(secret_set_request(secret, clear))
        .await
        .map_err(TranscriptionSecretError::Client)?;
    present_from_set_response(body)
}
