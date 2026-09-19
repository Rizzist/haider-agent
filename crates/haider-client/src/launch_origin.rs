//! Client-side launch-origin sanitisation
//! (`docs/design/dated-workspace-v1.md` §4, O6).
//!
//! The actual launch path lives only in client memory; component-aware
//! redaction runs BEFORE transmission or persistence, and the daemon
//! revalidates on ingress. Redacted labels are display data, never
//! filesystem arguments.

#[cfg(test)]
#[path = "launch_origin_tests.rs"]
mod tests;

use std::path::{Component, Path};

/// Upper bound on the serialized origin display (UTF-8 bytes). Over-limit
/// registration is rejected with a visible notice, not truncated silently.
pub const ORIGIN_DISPLAY_MAX_BYTES: usize = 4096;

/// Sanitised origin path, mirroring `haider-protocol`'s wire kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedOriginPath {
    pub kind: OriginPathKind,
    /// Optional only for [`OriginPathKind::Unavailable`].
    pub display: Option<String>,
}

/// Wire vocabulary for the origin path (O6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginPathKind {
    /// Own home or a descendant, shown as `~` / `~/suffix`.
    HomeRelative,
    /// An absolute path outside any recognised home.
    Absolute,
    /// Another user's home form with the user component masked.
    Redacted,
    /// The launch directory could not be captured.
    Unavailable,
}

impl OriginPathKind {
    /// The wire string for this kind.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::HomeRelative => "home_relative",
            Self::Absolute => "absolute",
            Self::Redacted => "redacted",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Sanitisation refusals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginSanitizeError {
    /// The escaped display exceeds [`ORIGIN_DISPLAY_MAX_BYTES`].
    DisplayTooLong(usize),
}

impl std::fmt::Display for OriginSanitizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DisplayTooLong(bytes) => write!(
                f,
                "origin display is {bytes} bytes; limit is {ORIGIN_DISPLAY_MAX_BYTES}"
            ),
        }
    }
}

impl std::error::Error for OriginSanitizeError {}

/// Sanitises a captured launch path against the resolved home directory.
///
/// - `None` becomes `unavailable` with no display.
/// - Own home and descendants become `~`/`~/suffix` using exact COMPONENT
///   prefix checks (`/Users/alice2` is never inside `/Users/alice`).
/// - Known other-user home forms (`/Users/<name>`, `/home/<name>`,
///   Windows `Users\<name>`) mask the user component as `<user>` and use
///   kind `redacted`.
/// - Control characters, escape/newline bytes, and bidi controls are
///   escaped as `\u{XXXX}`; distinct filenames stay distinct.
pub fn sanitize_origin_path(
    path: Option<&Path>,
    home: Option<&Path>,
) -> Result<SanitizedOriginPath, OriginSanitizeError> {
    let Some(path) = path else {
        return Ok(SanitizedOriginPath {
            kind: OriginPathKind::Unavailable,
            display: None,
        });
    };

    if let Some(home) = home
        && let Some(relative) = relative_to_own_home(path, home)
    {
        let display = if relative.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", escape_display(&path_to_display(&relative)))
        };
        return bounded(OriginPathKind::HomeRelative, display);
    }

    if let Some(masked) = mask_other_user_home(path) {
        return bounded(OriginPathKind::Redacted, escape_display(&masked));
    }

    bounded(
        OriginPathKind::Absolute,
        escape_display(&path_to_display(path)),
    )
}

/// Returns the component-relative own-home suffix using the caller's lexical
/// spelling first, then canonical identities. The latter covers a symlinked
/// HOME whose captured cwd has already been canonicalized by the OS/client.
fn relative_to_own_home(path: &Path, home: &Path) -> Option<std::path::PathBuf> {
    if let Ok(relative) = path.strip_prefix(home) {
        return Some(relative.to_path_buf());
    }
    let canonical_home = home.canonicalize().ok()?;
    let canonical_path = path.canonicalize().ok()?;
    canonical_path
        .strip_prefix(canonical_home)
        .ok()
        .map(Path::to_path_buf)
}

fn bounded(
    kind: OriginPathKind,
    display: String,
) -> Result<SanitizedOriginPath, OriginSanitizeError> {
    if display.len() > ORIGIN_DISPLAY_MAX_BYTES {
        return Err(OriginSanitizeError::DisplayTooLong(display.len()));
    }
    Ok(SanitizedOriginPath {
        kind,
        display: Some(display),
    })
}

fn path_to_display(path: &Path) -> String {
    // Lossy is acceptable for a display-only value; the workspace authority
    // uses canonical paths elsewhere.
    let mut text = String::new();
    let mut first = true;
    for component in path.components() {
        match component {
            Component::RootDir => {
                text.push('/');
                first = false;
                continue;
            }
            Component::Prefix(prefix) => {
                text.push_str(&prefix.as_os_str().to_string_lossy());
                first = false;
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                if !first && !text.ends_with('/') {
                    text.push('/');
                }
                text.push_str("..");
                first = false;
            }
            Component::Normal(part) => {
                if !first && !text.ends_with('/') {
                    text.push('/');
                }
                text.push_str(&part.to_string_lossy());
                first = false;
            }
        }
    }
    if text.is_empty() {
        path.to_string_lossy().into_owned()
    } else {
        text
    }
}

/// Masks `<name>` in the recognised other-user home forms; returns `None`
/// when the path does not match one.
fn mask_other_user_home(path: &Path) -> Option<String> {
    let display = path_to_display(path);
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = display.strip_prefix(prefix) {
            let mut split = rest.splitn(2, '/');
            let user = split.next().unwrap_or("");
            if user.is_empty() {
                return None;
            }
            let suffix = split.next();
            return Some(match suffix {
                Some(suffix) => format!("{prefix}<user>/{suffix}"),
                None => format!("{prefix}<user>"),
            });
        }
    }
    // Windows `<drive>:\Users\<name>` arrives here with normalized
    // component separators (`C:/Users/name`).
    if let Some(index) = display.find(":/Users/") {
        let (head, tail) = display.split_at(index + ":/Users/".len());
        let mut split = tail.splitn(2, '/');
        let user = split.next().unwrap_or("");
        if user.is_empty() {
            return None;
        }
        return Some(match split.next() {
            Some(suffix) => format!("{head}<user>/{suffix}"),
            None => format!("{head}<user>"),
        });
    }
    None
}

/// Escapes control characters, terminal escapes, newlines, and Unicode
/// bidi controls as `\u{XXXX}` so a path can never smuggle terminal or
/// prompt-direction tricks. A literal backslash is escaped too, so two
/// distinct filenames can never collapse into the same display text.
fn escape_display(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        let code = character as u32;
        let is_bidi = matches!(
            code,
            0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
        );
        if character.is_control() || is_bidi || character == '\\' {
            use std::fmt::Write;
            let _ = write!(escaped, "\\u{{{code:04x}}}");
        } else {
            escaped.push(character);
        }
    }
    escaped
}
