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
//! - Every profile receives a distinct runtime directory. `HAIDER_RUNTIME_DIR`
//!   selects the containing root for gates/CI; otherwise a verified
//!   owner-private `XDG_RUNTIME_DIR` on Linux or the resolved user home
//!   supplies the preferred base.
//! - The complete bind/staging path budget is checked during profile
//!   resolution. Unix falls back to a short owner- and profile-scoped `/tmp`
//!   path when the preferred socket path is too long. Windows uses a
//!   profile-digested named pipe and keeps filesystem runtime state under the
//!   selected root. Other endpoint failures remain typed and loud.
//! - The default model is a release-owned FULL Anthropic model ID: profile
//!   config (`config.json`) or `HAIDER_MODEL` may override the packaged
//!   value; the TUI's short product labels never enter this seam.

use std::path::{Path, PathBuf};

/// Environment variable naming the profile store directory.
pub const PROFILE_DIR_ENV: &str = "HAIDER_PROFILE_DIR";
/// Environment variable overriding the root that contains per-profile
/// runtime directories.
pub const RUNTIME_DIR_ENV: &str = "HAIDER_RUNTIME_DIR";
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
const RUNTIME_PROFILE_ID_CHARS: usize = 20;

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
    /// `USERPROFILE` (the standard Windows user-home variable).
    pub user_profile: Option<PathBuf>,
    /// `HAIDER_MODEL`.
    pub model: Option<String>,
    /// `HAIDER_RUNTIME_DIR`, interpreted as a root; the resolver always adds
    /// a profile-derived child so two profiles cannot share runtime files.
    pub runtime_dir: Option<PathBuf>,
    /// `XDG_RUNTIME_DIR` (consulted on Linux only).
    pub xdg_runtime_dir: Option<PathBuf>,
}

impl ProfileEnv {
    /// Snapshots the real process environment.
    pub fn capture() -> Self {
        Self {
            profile_dir: std::env::var_os(PROFILE_DIR_ENV).map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
            user_profile: std::env::var_os("USERPROFILE").map(PathBuf::from),
            model: std::env::var(MODEL_ENV)
                .ok()
                .filter(|m| !m.trim().is_empty()),
            runtime_dir: std::env::var_os(RUNTIME_DIR_ENV).map(PathBuf::from),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        }
    }
}

/// Platform temp inputs captured separately so tests can exercise base
/// selection without mutating process-global environment variables.
#[derive(Debug, Clone, Default)]
struct RuntimeEnv {
    #[cfg(unix)]
    tmpdir: Option<PathBuf>,
    #[cfg(unix)]
    prefix: Option<PathBuf>,
}

impl RuntimeEnv {
    fn capture() -> Self {
        Self {
            #[cfg(unix)]
            tmpdir: std::env::var_os("TMPDIR").map(PathBuf::from),
            #[cfg(unix)]
            prefix: std::env::var_os("PREFIX").map(PathBuf::from),
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
    /// Owner-private runtime directory holding PID/temp state and, on Unix,
    /// the rendezvous socket.
    pub runtime_dir: PathBuf,
    /// Deterministic rendezvous address: a socket under `runtime_dir` on Unix,
    /// or a profile-digested named-pipe address on Windows.
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
    /// Neither `HAIDER_PROFILE_DIR` nor a platform home variable is available.
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
    /// The selected runtime cannot be represented by the platform IPC API.
    /// This remains loud after both the preferred location and the bounded
    /// owner/profile fallback have been considered.
    RuntimeEndpoint {
        source: haider_platform::EndpointError,
    },
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStoreDir => formatter.write_str(profile_store_unavailable_message()),
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
                write!(
                    formatter,
                    "invalid profile config {}: {message}",
                    path.display()
                )
            }
            Self::RuntimeEndpoint { source } => write!(
                formatter,
                "profile runtime endpoint is not usable: {source}; set {RUNTIME_DIR_ENV} to a shorter owner-private directory"
            ),
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::RuntimeEndpoint { source } => Some(source),
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
        None => profile_home(env)
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

    let (runtime_dir, endpoint) = runtime_endpoint(env, &RuntimeEnv::capture(), &profile_id)?;
    let endpoint_path = endpoint.into_address();
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

#[cfg(target_os = "windows")]
fn profile_home(env: &ProfileEnv) -> Option<&Path> {
    env.user_profile
        .as_deref()
        .filter(|home| !home.as_os_str().is_empty())
        .or_else(|| {
            env.home
                .as_deref()
                .filter(|home| !home.as_os_str().is_empty())
        })
}

#[cfg(not(target_os = "windows"))]
fn profile_home(env: &ProfileEnv) -> Option<&Path> {
    env.home
        .as_deref()
        .filter(|home| !home.as_os_str().is_empty())
}

#[cfg(target_os = "windows")]
fn profile_store_unavailable_message() -> &'static str {
    "cannot resolve a profile directory: HAIDER_PROFILE_DIR is unset and USERPROFILE/HOME are unavailable"
}

#[cfg(not(target_os = "windows"))]
fn profile_store_unavailable_message() -> &'static str {
    "cannot resolve a profile directory: HAIDER_PROFILE_DIR is unset and HOME is unavailable"
}

/// The deterministic rendezvous socket path.
///
/// This derivation is the wire-level rendezvous law: `haider-daemon`'s
/// `DaemonConfig::endpoint_path` delegates here, so client and daemon can
/// never derive different socket names for the same profile. The containing
/// directory carries the profile scope, allowing a short fixed-size basename
/// under the tight Unix socket path limit (`sun_path`, ~104 bytes on macOS).
pub fn endpoint_path_for(runtime_dir: &Path, profile_id: &str) -> PathBuf {
    haider_platform::Endpoint::new(runtime_dir, profile_id).into_address()
}

fn runtime_endpoint(
    env: &ProfileEnv,
    runtime_env: &RuntimeEnv,
    profile_id: &str,
) -> Result<(PathBuf, haider_platform::Endpoint), ProfileError> {
    let preferred_runtime_dir = runtime_dir(env, runtime_env, profile_id);
    let preferred_endpoint = haider_platform::Endpoint::new(&preferred_runtime_dir, profile_id);
    match preferred_endpoint.validate_for_bind(&preferred_runtime_dir) {
        Ok(()) => return Ok((preferred_runtime_dir, preferred_endpoint)),
        Err(haider_platform::EndpointError::AddressTooLong { .. }) => {}
        Err(source) => return Err(ProfileError::RuntimeEndpoint { source }),
    }

    let fallback_runtime_dir = short_runtime_dir(profile_id);
    let fallback_endpoint = haider_platform::Endpoint::new(&fallback_runtime_dir, profile_id);
    fallback_endpoint
        .validate_for_bind(&fallback_runtime_dir)
        .map_err(|source| ProfileError::RuntimeEndpoint { source })?;
    Ok((fallback_runtime_dir, fallback_endpoint))
}

/// Resolves the runtime directory from verified platform temp bases.
fn runtime_dir(env: &ProfileEnv, runtime_env: &RuntimeEnv, profile_id: &str) -> PathBuf {
    let root = runtime_root(env, runtime_env);
    let scope = profile_id
        .chars()
        .take(RUNTIME_PROFILE_ID_CHARS)
        .collect::<String>();
    haider_platform::owner_scoped_runtime_directory(&root.join(scope))
}

#[cfg(unix)]
fn short_runtime_dir(profile_id: &str) -> PathBuf {
    let scope = profile_id
        .chars()
        .take(RUNTIME_PROFILE_ID_CHARS)
        .collect::<String>();
    PathBuf::from("/tmp")
        .join(format!("haider-{}", effective_uid()))
        .join(scope)
}

#[cfg(windows)]
fn short_runtime_dir(profile_id: &str) -> PathBuf {
    let scope = profile_id
        .chars()
        .take(RUNTIME_PROFILE_ID_CHARS)
        .collect::<String>();
    std::env::temp_dir().join("haider").join(scope)
}

fn runtime_root(env: &ProfileEnv, runtime_env: &RuntimeEnv) -> PathBuf {
    if let Some(override_root) = &env.runtime_dir
        && !override_root.as_os_str().is_empty()
    {
        return override_root.clone();
    }
    #[cfg(target_os = "linux")]
    if let Some(xdg) = &env.xdg_runtime_dir
        && verified_owner_private(xdg)
    {
        return xdg.join("haider");
    }
    if let Some(home) = profile_home(env) {
        return home.join(".haider").join("runtime");
    }
    #[cfg(unix)]
    {
        if let Some(tmpdir) = &runtime_env.tmpdir
            && verified_owner_private(tmpdir)
        {
            return tmpdir.join("haider");
        }
        if let Some(prefix) = &runtime_env.prefix {
            let prefix_tmp = prefix.join("tmp");
            if verified_owner_private(&prefix_tmp) {
                return prefix_tmp.join("haider");
            }
        }
        PathBuf::from("/tmp").join(format!("haider-{}", effective_uid()))
    }
    #[cfg(windows)]
    {
        let _ = runtime_env;
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

/// A directory qualifies as a Unix runtime base only when it is a real
/// directory owned by this UID with no group/other access.
#[cfg(unix)]
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
