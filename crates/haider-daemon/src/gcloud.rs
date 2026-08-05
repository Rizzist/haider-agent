//! The gcloud access-token refresh source (G4b, LV2).
//!
//! Vertex Bearer tokens expire in ~1 hour. The D-wave device-credential
//! pattern applies: a discovery candidate advertises the local gcloud ADC
//! installation, import shells out to `gcloud auth print-access-token` and
//! vaults the RESULT (never a durable secret file copy), and the broker
//! re-runs the same command when a turn fails authentication. The shell-out
//! is a trait so tests mock it — no production command ever runs in a test.

use haider_protocol::error::{ErrorCode, HaiderError};
use zeroize::Zeroizing;

/// Where the vertex gcloud-refresh credential lives in the vault. The alias
/// is the REFRESH MARKER: only a descriptor at exactly this coordinate is
/// ever re-minted through the shell-out, so a pasted-token account can never
/// be silently overwritten by gcloud.
pub(crate) const VERTEX_GCLOUD_ALIAS: &str = "vertex-gcloud";

/// Bound on the token/stderr bytes read from the child — an access token is
/// well under this; anything larger is not a token.
const GCLOUD_OUTPUT_LIMIT: usize = 64 * 1024;

/// One mockable shell-out. The production implementation runs the REAL
/// `gcloud auth print-access-token`; tests inject scripted sources.
pub trait GcloudAccessTokenSource: Send + Sync {
    /// Returns the current ADC access token bytes (trimmed), or a typed,
    /// secret-free error naming what failed.
    fn print_access_token(&self) -> Result<Zeroizing<Vec<u8>>, HaiderError>;
}

/// Production source: `gcloud auth print-access-token` with no shell, no
/// argument interpolation, and bounded output.
pub struct GcloudCli;

impl GcloudAccessTokenSource for GcloudCli {
    fn print_access_token(&self) -> Result<Zeroizing<Vec<u8>>, HaiderError> {
        let output = std::process::Command::new("gcloud")
            .args(["auth", "print-access-token"])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| gcloud_error(format!("could not run gcloud: {error}")))?;
        if !output.status.success() {
            // stderr is diagnostic prose (auth/login instructions), never
            // token material — bounded and lossy-decoded for the message.
            let stderr = String::from_utf8_lossy(
                &output.stderr[..output.stderr.len().min(GCLOUD_OUTPUT_LIMIT)],
            );
            let line = stderr.lines().find(|line| !line.trim().is_empty());
            return Err(gcloud_error(format!(
                "gcloud exited with {}: {}",
                output.status,
                line.unwrap_or("no diagnostic output").trim()
            )));
        }
        let mut stdout = Zeroizing::new(output.stdout);
        if stdout.len() > GCLOUD_OUTPUT_LIMIT {
            return Err(gcloud_error("gcloud output exceeds the token bound"));
        }
        let trimmed = crate::device_discovery::trim_ascii(&stdout);
        if trimmed.is_empty() {
            return Err(gcloud_error("gcloud printed no access token"));
        }
        let token = Zeroizing::new(trimmed.to_vec());
        stdout.iter_mut().for_each(|byte| *byte = 0);
        Ok(token)
    }
}

pub(crate) fn gcloud_error(message: impl Into<String>) -> HaiderError {
    HaiderError::new(
        ErrorCode::ProviderError,
        format!("gcloud auth print-access-token failed: {}", message.into()),
        false,
    )
}

/// Whether this descriptor is THE vertex gcloud-refresh credential (the
/// broker's refresh gate and the attempt resolver's retry gate share this
/// one predicate).
pub(crate) fn is_gcloud_refresh_descriptor(
    descriptor: &haider_protocol::credential::CredentialDescriptor,
) -> bool {
    descriptor.provider == haider_provider::VERTEX_PROVIDER_NAME
        && descriptor.alias.as_str() == VERTEX_GCLOUD_ALIAS
        && descriptor.auth_method == haider_protocol::credential::AuthMethod::ApiKey
}
