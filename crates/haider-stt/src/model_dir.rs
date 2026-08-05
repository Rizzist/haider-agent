//! ADE-byte-compatible shared model directory resolver.
//!
//! This is the byte-for-byte contract of locked decision 3: Haider resolves
//! the EXACT directory the Diff Forge ADE's `cloud_mcp_native_data_root()`
//! resolves (rust-diffforge `src-tauri/src/cloud_mcp.rs:1467-1503`), then
//! appends `whisper`. Any divergence — a different env precedence, a
//! capitalized `diffforge` on Linux, an appended suffix on the env overrides —
//! silently forks the model store and breaks model sharing, so the resolution
//! core is platform-parametric and pinned by literal-path tests for all three
//! platforms and both env overrides.
//!
//! ADE resolution order:
//! 1. `$RUST_DIFFFORGE_DATA_DIR` — used AS-IS (no suffix appended).
//! 2. `$RUST_DIFFFORGE_HOME` — used AS-IS.
//! 3. Platform default:
//!    - macOS: `$HOME/Library/Application Support/DiffForge`
//!    - Windows: `%APPDATA%\DiffForge`, falling back `%LOCALAPPDATA%\DiffForge`
//!    - Linux: `$XDG_DATA_HOME/diffforge`, falling back
//!      `$HOME/.local/share/diffforge` — LOWERCASE `diffforge` (the trap).
//!
//! Home is `$HOME` falling back `%USERPROFILE%` on every platform (ADE
//! `cloud_mcp_home_dir`). Empty env values are treated as unset (ADE
//! `cloud_mcp_env_path`).

use std::ffi::OsString;
use std::path::PathBuf;

/// First env override: an explicit data root, used verbatim.
pub const DATA_DIR_ENV: &str = "RUST_DIFFFORGE_DATA_DIR";
/// Second env override: the Diff Forge home root, used verbatim.
pub const HOME_ENV: &str = "RUST_DIFFFORGE_HOME";
/// Subdirectory of the data root holding whisper models and runtime.
pub const WHISPER_DIR_NAME: &str = "whisper";

/// Platform whose ADE path convention applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Windows,
    Linux,
}

impl Platform {
    /// The platform this build is running on. Every non-macOS, non-Windows
    /// unix follows the Linux (XDG, lowercase) convention — exactly the
    /// ADE's `#[cfg(all(unix, not(target_os = "macos")))]` arm.
    #[must_use]
    pub const fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(windows)]
        {
            Self::Windows
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Self::Linux
        }
    }
}

fn env_path(env: &dyn Fn(&str) -> Option<OsString>, key: &str) -> Option<PathBuf> {
    env(key)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn home_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env_path(env, "HOME").or_else(|| env_path(env, "USERPROFILE"))
}

/// Resolves the ADE data root for `platform` from an injected env snapshot.
///
/// Returns `None` only when no override is set and the platform has no home
/// (no `$HOME`/`%USERPROFILE%`, no `%APPDATA%`): an honest "nowhere to
/// share" state the caller reports, never a guessed path.
#[must_use]
pub fn resolve_data_root(
    platform: Platform,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = env_path(env, DATA_DIR_ENV) {
        return Some(path);
    }
    if let Some(path) = env_path(env, HOME_ENV) {
        return Some(path);
    }
    match platform {
        Platform::MacOs => home_dir(env).map(|home| {
            home.join("Library")
                .join("Application Support")
                .join("DiffForge")
        }),
        Platform::Windows => env_path(env, "APPDATA")
            .map(|appdata| appdata.join("DiffForge"))
            .or_else(|| env_path(env, "LOCALAPPDATA").map(|local| local.join("DiffForge"))),
        Platform::Linux => env_path(env, "XDG_DATA_HOME")
            .map(|xdg| xdg.join("diffforge"))
            .or_else(|| {
                home_dir(env).map(|home| home.join(".local").join("share").join("diffforge"))
            }),
    }
}

/// Resolves the shared whisper model directory (`<data_root>/whisper`).
#[must_use]
pub fn resolve_whisper_dir(
    platform: Platform,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    resolve_data_root(platform, env).map(|root| root.join(WHISPER_DIR_NAME))
}

/// The shared whisper model directory for THIS process (current platform,
/// process environment).
#[must_use]
pub fn whisper_dir() -> Option<PathBuf> {
    resolve_whisper_dir(Platform::current(), &|key| std::env::var_os(key))
}
