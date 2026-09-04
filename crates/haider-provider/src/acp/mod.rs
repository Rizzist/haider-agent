//! Agent Client Protocol (ACP) core for the supervised Google Antigravity
//! agent.
//!
//! Google owns the OAuth for the Gemini subscription: Haider does NOT
//! implement Google OAuth and never touches a Code Assist HTTP endpoint.
//! Instead it supervises Google's official `antigravity-acp` agent as a
//! subprocess and speaks ACP to it over stdio — newline-delimited JSON-RPC
//! 2.0, camelCase on the wire.
//!
//! Ground truth for every field name, method name, error code and framing rule
//! in this module is `docs/testing/v0.0.970/googleoauth_acp-wire-facts.md`, which was
//! extracted from the published v1 JSON schema and from a live handshake
//! against the real 1.1.1 binary.
//!
//! Module map:
//! - [`wire`] — serde types for the exact message set Haider speaks.
//! - [`codec`] — the newline-delimited framing and its derived bounds.
//! - [`client`] — the supervised connection: correlation, inbound-request
//!   handling, bounded stderr drain, child lifecycle.
//! - [`antigravity`] — the [`crate::Provider`] adapter and the session-update
//!   to `StreamEvent` mapping.

pub mod antigravity;
pub mod client;
pub mod codec;
pub mod wire;

/// Provider class name for the Google Antigravity (Gemini subscription)
/// adapter.
///
/// A member of [`crate::BUILTIN_PROVIDER_NAMES`] since v0.0.970, once the
/// pinned installer and the account-backed factory branch landed. It is a
/// SEPARATE class from the API-key `gemini` provider and never changes it.
pub const GOOGLE_ANTIGRAVITY_PROVIDER_NAME: &str = "google-antigravity";

pub use antigravity::{
    ACP_OAUTH_PERSONAL_METHOD_ID, AntigravityAcpProvider, AntigravitySessionConfig,
};
/// The model catalog is a session CONFIGURATION OPTION, not an ACP field of
/// its own, so its projection is re-exported here beside the adapter that
/// resolves it.
pub use wire::{ACP_MODEL_CONFIG_OPTION_ID, AcpModelCatalog, AcpModelOption};
