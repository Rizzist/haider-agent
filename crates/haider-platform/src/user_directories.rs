//! Platform Documents-directory convention for the dated workspace base
//! (`docs/design/dated-workspace-v1.md` §2, W6).
//!
//! Generated work is user content, so the default base follows each
//! platform's Documents convention rather than cache/runtime directories.
//! A lookup failure falls back to the resolved home WITH A RECORDED
//! REASON; it never silently spills into `/tmp`, `/`, or another drive.
//! Inaccessibility of a successfully selected location is the caller's
//! visible allocation error, not this module's concern.

#[cfg(test)]
#[path = "user_directories_tests.rs"]
mod tests;

use std::path::{Path, PathBuf};

/// Why the platform Documents lookup fell back to home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentsFallbackReason {
    /// The platform lookup itself was unavailable (no config file, missing
    /// key, unreadable home, or an OS API failure).
    LookupUnavailable,
    /// Linux `xdg-user-dirs` explicitly disables Documents by pointing the
    /// key at `$HOME`.
    Disabled,
    /// The configured value violated the specification (relative path,
    /// unquoted form, or embedded shell syntax) and was refused as data.
    InvalidValue,
}

/// Result of the Documents-convention lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentsLookup {
    /// The platform's Documents directory.
    Documents(PathBuf),
    /// The resolved home directory, with the reason Documents was not used.
    HomeFallback {
        home: PathBuf,
        reason: DocumentsFallbackReason,
    },
}

impl DocumentsLookup {
    /// The directory to use as the workspace base parent.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Documents(path) => path,
            Self::HomeFallback { home, .. } => home,
        }
    }
}

/// Parses one `user-dirs.dirs` file for `XDG_DOCUMENTS_DIR` AS DATA.
///
/// Accepted values per the xdg-user-dirs format: `"$HOME/relative"` or an
/// absolute quoted path. The file is never sourced as shell code; any
/// other syntax is refused. `"$HOME"` exactly means Documents is disabled.
/// Pure and platform-independent so every host can test it.
#[must_use]
pub fn parse_xdg_documents_dir(content: &str, home: &Path) -> Option<DocumentsLookup> {
    let mut result = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("XDG_DOCUMENTS_DIR") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        // The format requires a double-quoted value.
        let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
            result = Some(DocumentsLookup::HomeFallback {
                home: home.to_path_buf(),
                reason: DocumentsFallbackReason::InvalidValue,
            });
            continue;
        };
        // Never interpret embedded expansion/quoting as shell.
        if inner.contains('"') || inner.contains('`') || inner.contains('\\') {
            result = Some(DocumentsLookup::HomeFallback {
                home: home.to_path_buf(),
                reason: DocumentsFallbackReason::InvalidValue,
            });
            continue;
        }
        if inner == "$HOME" || inner == "$HOME/" {
            result = Some(DocumentsLookup::HomeFallback {
                home: home.to_path_buf(),
                reason: DocumentsFallbackReason::Disabled,
            });
            continue;
        }
        if let Some(suffix) = inner.strip_prefix("$HOME/") {
            if suffix.contains('$') {
                result = Some(DocumentsLookup::HomeFallback {
                    home: home.to_path_buf(),
                    reason: DocumentsFallbackReason::InvalidValue,
                });
            } else {
                result = Some(DocumentsLookup::Documents(home.join(suffix)));
            }
            continue;
        }
        if inner.starts_with('/') && !inner.contains('$') {
            result = Some(DocumentsLookup::Documents(PathBuf::from(inner)));
            continue;
        }
        // Relative paths and any other `$VAR` form violate the spec.
        result = Some(DocumentsLookup::HomeFallback {
            home: home.to_path_buf(),
            reason: DocumentsFallbackReason::InvalidValue,
        });
    }
    result
}

/// Linux lookup: `XDG_DOCUMENTS_DIR` from
/// `${XDG_CONFIG_HOME:-$HOME/.config}/user-dirs.dirs`, parameterized for
/// tests. A relative `XDG_CONFIG_HOME` is ignored per the XDG basedir
/// specification.
#[must_use]
pub fn linux_documents_directory(
    home: &Path,
    xdg_config_home: Option<&Path>,
    read_config: impl Fn(&Path) -> Option<String>,
) -> DocumentsLookup {
    let config_dir = match xdg_config_home {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        _ => home.join(".config"),
    };
    let Some(content) = read_config(&config_dir.join("user-dirs.dirs")) else {
        return DocumentsLookup::HomeFallback {
            home: home.to_path_buf(),
            reason: DocumentsFallbackReason::LookupUnavailable,
        };
    };
    parse_xdg_documents_dir(&content, home).unwrap_or(DocumentsLookup::HomeFallback {
        home: home.to_path_buf(),
        reason: DocumentsFallbackReason::LookupUnavailable,
    })
}

/// The current platform's Documents convention for the given resolved home.
///
/// macOS keeps the user-domain `~/Documents` location (v1 deliberately
/// avoids linking a UI framework for the Foundation lookup; a user who has
/// relocated Documents can set an explicit base). Linux consults
/// `xdg-user-dirs`. Windows asks the OS for the known folder so
/// redirected/OneDrive Documents are preserved. Android never calls this:
/// the app-private workspace ceiling wins before base resolution.
#[must_use]
pub fn documents_directory(home: &Path) -> DocumentsLookup {
    #[cfg(target_os = "macos")]
    {
        DocumentsLookup::Documents(home.join("Documents"))
    }
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
    {
        linux_documents_directory(
            home,
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .as_deref(),
            |path| std::fs::read_to_string(path).ok(),
        )
    }
    #[cfg(target_os = "android")]
    {
        DocumentsLookup::HomeFallback {
            home: home.to_path_buf(),
            reason: DocumentsFallbackReason::LookupUnavailable,
        }
    }
    #[cfg(windows)]
    {
        windows_documents_directory(home)
    }
}

/// Windows: `SHGetKnownFolderPath(FOLDERID_Documents)` for the current
/// user with no creation/redirection flags, preserving redirected,
/// OneDrive, and UNC known-folder locations. Lookup failure falls back to
/// the resolved home (which the caller derived from the profile resolver's
/// `USERPROFILE`/`HOME` rules); nothing is hardcoded.
#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_documents_directory(home: &Path) -> DocumentsLookup {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};

    let mut buffer: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: the API contract is an out-pointer that, on success, owns a
    // COM allocation which must be released with `CoTaskMemFree` on every
    // path once consumed.
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut buffer) };
    if result != 0 || buffer.is_null() {
        return DocumentsLookup::HomeFallback {
            home: home.to_path_buf(),
            reason: DocumentsFallbackReason::LookupUnavailable,
        };
    }
    // SAFETY: on success the buffer is a NUL-terminated UTF-16 string owned
    // by this frame; it is measured, copied, then freed exactly once.
    let path = unsafe {
        let mut length = 0usize;
        while *buffer.add(length) != 0 {
            length += 1;
        }
        let slice = std::slice::from_raw_parts(buffer, length);
        let path = PathBuf::from(std::ffi::OsString::from_wide(slice));
        CoTaskMemFree(buffer.cast());
        path
    };
    if path.as_os_str().is_empty() {
        return DocumentsLookup::HomeFallback {
            home: home.to_path_buf(),
            reason: DocumentsFallbackReason::LookupUnavailable,
        };
    }
    DocumentsLookup::Documents(path)
}
