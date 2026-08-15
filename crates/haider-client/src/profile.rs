//! The one shared profile resolver (report R8).
//!
//! `haider` and `haiderd` MUST resolve identity through this exact module —
//! never through a re-derived path — so the client's endpoint and the
//! daemon's endpoint can never disagree. Laws:
//!
//! - `HAIDER_PROFILE_DIR` and the historical default store directory
//!   (`$HOME/.haider/dev-profile`) are preserved.
//! - The store directory is made absolute, created if absent, then
//!   canonicalized; `profile_id` is the lowercase BLAKE3 hex digest of a
//!   version tag plus the canonical store-path bytes.
//! - The runtime directory is NEVER environment-overridable
//!   (`HAIDER_RUNTIME_DIR` is deliberately not read — D1's short
//!   private-directory rule): on Linux a verified owner-private
//!   `XDG_RUNTIME_DIR/haider` is used when available, otherwise (and always
//!   on macOS) the short constant base `/tmp` plus `haider-<effective-uid>`.
//! - The default model is a release-owned FULL Anthropic model ID: profile
//!   config (`config.json`) or `HAIDER_MODEL` may override the packaged
//!   value; the TUI's short product labels never enter this seam.

use std::path::{Path, PathBuf};

/// Environment variable naming the profile store directory.
pub const PROFILE_DIR_ENV: &str = "HAIDER_PROFILE_DIR";
/// Environment variable overriding the default full model ID.
pub const MODEL_ENV: &str = "HAIDER_MODEL";
/// Optional per-profile configuration file inside the store directory.
pub const PROFILE_CONFIG_FILE: &str = "config.json";
/// W3c default provider for new sessions.
pub const DEFAULT_PROVIDER: &str = "anthropic";
/// W3c default max output tokens for new sessions.
pub const DEFAULT_MAX_TOKENS: u64 = 4096;
/// Release-owned packaged default: a FULL Anthropic model ID (never a short
/// product label). Verified by the ignored live smoke, which is evidence,
/// never the merge gate.
pub const PACKAGED_DEFAULT_MODEL: &str = "claude-opus-5";

/// Domain-separation tag hashed ahead of the canonical store path.
const PROFILE_ID_TAG: &[u8] = b"haider-profile-id-v1\n";

/// Captured environment inputs to profile resolution.
///
/// Captured explicitly (not read ambiently inside the resolver) so tests can
/// construct arbitrary environments without mutating process env, which is
/// unsafe under Rust 2024 and racy under parallel tests.
#[derive(Debug, Clone, Default)]
pub struct ProfileEnv {
    /// `HAIDER_PROFILE_DIR`.
    pub profile_dir: Option<PathBuf>,
    /// `HOME`.
    pub home: Option<PathBuf>,
    /// `HAIDER_MODEL`.
    pub model: Option<String>,
    /// `XDG_RUNTIME_DIR` (consulted on Linux only).
    pub xdg_runtime_dir: Option<PathBuf>,
}

impl ProfileEnv {
    /// Snapshots the real process environment.
    pub fn capture() -> Self {
        Self {
            profile_dir: std::env::var_os(PROFILE_DIR_ENV).map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
            model: std::env::var(MODEL_ENV)
                .ok()
                .filter(|m| !m.trim().is_empty()),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        }
    }
}

/// The one shared profile identity both `haider` and `haiderd` resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// Lowercase BLAKE3 hex digest of the version tag + canonical store path.
    pub profile_id: String,
    /// Canonical absolute store directory (SQLite journal, lock, accounts).
    pub store_dir: PathBuf,
    /// Owner-private runtime directory holding the rendezvous socket.
    pub runtime_dir: PathBuf,
    /// Deterministic rendezvous socket path under `runtime_dir`.
    pub endpoint_path: PathBuf,
    /// Default provider for new sessions.
    pub default_provider: String,
    /// Release-owned full model ID for new sessions and login validation.
    pub default_model: String,
    /// Default max output tokens for new sessions.
    pub default_max_tokens: u64,
}

/// Typed profile-resolution failure.
#[derive(Debug)]
pub enum ProfileError {
    /// Neither `HAIDER_PROFILE_DIR` nor `HOME` is available.
    NoStoreDir,
    /// A filesystem step failed at a named path.
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// The canonical store path is not valid UTF-8 (the profile config and
    /// diagnostics require a printable identity).
    NonUtf8Path(PathBuf),
    /// `config.json` exists but cannot be parsed; a malformed config must be
    /// loud, never silently ignored.
    InvalidConfig { path: PathBuf, message: String },
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStoreDir => formatter.write_str(
                "cannot resolve a profile directory: HAIDER_PROFILE_DIR is unset and HOME is unavailable",
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
            Self::NonUtf8Path(path) => write!(
                formatter,
                "canonical profile path is not valid UTF-8: {}",
                path.display()
            ),
            Self::InvalidConfig { path, message } => {
                write!(formatter, "invalid profile config {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Optional keys of `<store_dir>/config.json`.
#[derive(Debug, Default, serde::Deserialize)]
struct ProfileConfig {
    #[serde(default)]
    default_model: Option<String>,
}

/// Resolves the shared profile from an explicit environment snapshot.
pub fn resolve_profile(env: &ProfileEnv) -> Result<ResolvedProfile, ProfileError> {
    let store_dir = match &env.profile_dir {
        Some(dir) => dir.clone(),
        None => env
            .home
            .as_ref()
            .filter(|home| !home.as_os_str().is_empty())
            .map(|home| home.join(".haider").join("dev-profile"))
            .ok_or(ProfileError::NoStoreDir)?,
    };
    let store_dir = absolute(store_dir)?;
    std::fs::create_dir_all(&store_dir).map_err(|source| ProfileError::Io {
        operation: "create profile store directory",
        path: store_dir.clone(),
        source,
    })?;
    let store_dir = store_dir
        .canonicalize()
        .map_err(|source| ProfileError::Io {
            operation: "canonicalize profile store directory",
            path: store_dir.clone(),
            source,
        })?;
    let store_text = store_dir
        .to_str()
        .ok_or_else(|| ProfileError::NonUtf8Path(store_dir.clone()))?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(PROFILE_ID_TAG);
    hasher.update(store_text.as_bytes());
    let profile_id = hasher.finalize().to_hex().to_string();

    let runtime_dir = runtime_dir(env);
    let endpoint_path = endpoint_path_for(&runtime_dir, &profile_id);
    let default_model = resolve_default_model(&store_dir, env)?;

    Ok(ResolvedProfile {
        profile_id,
        store_dir,
        runtime_dir,
        endpoint_path,
        default_provider: DEFAULT_PROVIDER.to_owned(),
        default_model,
        default_max_tokens: DEFAULT_MAX_TOKENS,
    })
}

/// The deterministic rendezvous socket path.
///
/// This derivation is the wire-level rendezvous law: `haider-daemon`'s
/// `DaemonConfig::endpoint_path` delegates here, so client and daemon can
/// never derive different socket names for the same profile. The digest keeps
/// the name a constant 32 hex chars regardless of profile-id length, which
/// protects the tight OS limit on Unix socket path length (`sun_path`,
/// ~104 bytes on macOS).
pub fn endpoint_path_for(runtime_dir: &Path, profile_id: &str) -> PathBuf {
    haider_platform::Endpoint::new(runtime_dir, profile_id).into_address()
}

/// Resolves the runtime directory (never environment-overridable).
fn runtime_dir(env: &ProfileEnv) -> PathBuf {
    #[cfg(target_os = "linux")]
    if let Some(xdg) = &env.xdg_runtime_dir
        && verified_owner_private(xdg)
    {
        return xdg.join("haider");
    }
    #[cfg(all(not(target_os = "linux"), unix))]
    let _ = env;
    #[cfg(unix)]
    return PathBuf::from("/tmp").join(format!("haider-{}", effective_uid()));
    #[cfg(windows)]
    {
        let _ = env;
        std::env::temp_dir().join("haider")
    }
}

/// Effective UID of this process.
#[cfg(unix)]
pub fn effective_uid() -> u32 {
    haider_platform::effective_user_id()
}

/// Windows has no Unix effective UID. The endpoint is protected by its named
/// pipe DACL rather than this compatibility value.
#[cfg(windows)]
pub fn effective_uid() -> u32 {
    0
}

/// A directory qualifies as an XDG runtime base only when it is a real
/// directory owned by this UID with no group/other access.
#[cfg(target_os = "linux")]
fn verified_owner_private(path: &Path) -> bool {
    haider_platform::is_owner_private_directory(path)
}

/// Resolves the release-owned default model for an EXPLICIT store directory
/// (the `haiderd --store-dir …` path): identical precedence to
/// [`resolve_profile`] — `HAIDER_MODEL`, then `config.json`, then the
/// packaged constant.
pub fn resolve_default_model_for(
    store_dir: &Path,
    env: &ProfileEnv,
) -> Result<String, ProfileError> {
    resolve_default_model(store_dir, env)
}

fn resolve_default_model(store_dir: &Path, env: &ProfileEnv) -> Result<String, ProfileError> {
    if let Some(model) = &env.model {
        return Ok(model.clone());
    }
    let config_path = store_dir.join(PROFILE_CONFIG_FILE);
    match std::fs::read_to_string(&config_path) {
        Ok(text) => {
            let config: ProfileConfig =
                serde_json::from_str(&text).map_err(|error| ProfileError::InvalidConfig {
                    path: config_path.clone(),
                    message: error.to_string(),
                })?;
            match config
                .default_model
                .filter(|model| !model.trim().is_empty())
            {
                Some(model) => Ok(model),
                None => Ok(PACKAGED_DEFAULT_MODEL.to_owned()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PACKAGED_DEFAULT_MODEL.to_owned())
        }
        Err(source) => Err(ProfileError::Io {
            operation: "read profile config",
            path: config_path,
            source,
        }),
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf, ProfileError> {
    if path.is_absolute() {
        return Ok(path);
    }
    let cwd = std::env::current_dir().map_err(|source| ProfileError::Io {
        operation: "resolve current directory for relative profile path",
        path: path.clone(),
        source,
    })?;
    Ok(cwd.join(path))
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod profile_tests;
