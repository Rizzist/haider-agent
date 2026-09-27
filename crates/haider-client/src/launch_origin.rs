//! Display-only projection of filesystem paths. Never pass its output to IO.

#[cfg(test)]
#[path = "launch_origin_tests.rs"]
mod tests;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const ORIGIN_DISPLAY_MAX_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedOriginPath {
    pub kind: OriginPathKind,
    pub display: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginPathKind {
    HomeRelative,
    Absolute,
    Redacted,
    Unavailable,
}

impl OriginPathKind {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginSanitizeError {
    DisplayTooLong(usize),
}

pub fn random_open_namespace() -> std::io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| std::io::Error::other(format!("origin entropy unavailable: {error}")))?;
    let mut namespace = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(namespace, "{byte:02x}");
    }
    Ok(namespace)
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

/// A usable home must be absolute and contain a real directory component.
/// This also accepts Windows paths in host-simulated tests on Unix.
#[must_use]
pub fn usable_home(home: &Path) -> bool {
    usable_home_on(home, cfg!(windows))
}

fn usable_home_on(home: &Path, windows: bool) -> bool {
    let parsed = DisplayPath::parse(home);
    parsed.absolute
        && (!windows || parsed.root.ends_with(':') || parsed.root.starts_with("//"))
        && !parsed.parts.is_empty()
        && !parsed.unresolved_parent
        && !parsed.is_multiuser_ancestor()
        && !(cfg!(unix)
            && parsed.root == "/"
            && home
                .canonicalize()
                .ok()
                .is_some_and(|canonical| DisplayPath::parse(&canonical).is_multiuser_ancestor()))
}

/// The single path projection. HOME precedes USERPROFILE on equal or nested
/// matches; the longest matching home wins when both are configured.
pub fn sanitize_origin_path_with_homes(
    path: Option<&Path>,
    homes: &[&Path],
) -> Result<SanitizedOriginPath, OriginSanitizeError> {
    sanitize_with_canonicalizers(
        path,
        homes,
        &canonicalize_existing_prefix,
        &canonicalize_masking_prefix,
    )
}

/// Display-only path label for CLI and TUI chrome. The real `Path` stays
/// with its caller for every filesystem or daemon operation.
#[must_use]
pub fn display_path(path: &Path) -> String {
    display_path_with_homes(path, &process_display_homes())
}

/// Injectable form of the one display producer for host-simulated tests.
#[must_use]
pub fn display_path_with_homes(path: &Path, homes: &[PathBuf]) -> String {
    sanitize_origin_path_with_homes(
        Some(path),
        &homes.iter().map(PathBuf::as_path).collect::<Vec<_>>(),
    )
    .ok()
    .and_then(|sanitized| sanitized.display)
    .unwrap_or_else(|| "<workspace path unavailable>".to_owned())
}

/// Compatibility entry point for callers with one known home.
pub fn sanitize_origin_path(
    path: Option<&Path>,
    home: Option<&Path>,
) -> Result<SanitizedOriginPath, OriginSanitizeError> {
    sanitize_origin_path_with_homes(path, &home.into_iter().collect::<Vec<_>>())
}

/// HOME and, on Windows, USERPROFILE. The profile snapshot is the sole
/// environment reader for display home selection.
#[must_use]
pub fn process_display_homes() -> Vec<PathBuf> {
    let env = crate::profile::ProfileEnv::capture();
    display_homes_from_profile(&env, cfg!(windows))
}

fn display_homes_from_profile(env: &crate::profile::ProfileEnv, windows: bool) -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let profile = env
        .user_profile
        .as_ref()
        .filter(|home| usable_home_on(home, windows));
    if let Some(home) = env.home.as_ref()
        && (usable_home_on(home, windows)
            || (windows && profile.is_some_and(|profile| redirected_home_root(home, profile))))
    {
        homes.push(home.clone());
    }
    if windows && let Some(profile) = profile {
        homes.push(profile.clone());
    }
    homes
}

/// A redirected Windows HOME may be the root of a mapped drive or UNC share. Accept that
/// root only alongside a distinct, valid USERPROFILE, never as the sole home
/// or as the root of the profile's own volume/share.
fn redirected_home_root(home: &Path, profile: &Path) -> bool {
    let home = DisplayPath::parse(home);
    let profile_usable = usable_home(profile);
    let profile = DisplayPath::parse(profile);
    let admin_share = home.root.rsplit('/').next().is_some_and(|share| {
        let bytes = share.as_bytes();
        (bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b'$')
            || share.eq_ignore_ascii_case("admin$")
            || share.eq_ignore_ascii_case("ipc$")
    });
    home.absolute
        && home.windows
        && (home.root.ends_with(':') || home.root.starts_with("//"))
        && !admin_share
        && home.parts.is_empty()
        && profile_usable
        && profile.windows
        && !home.root.eq_ignore_ascii_case(&profile.root)
}

/// Used by workspace configuration when choosing one home, without changing
/// the display rule that accepts both Windows homes.
#[must_use]
pub fn preferred_process_home() -> Option<PathBuf> {
    process_display_homes().into_iter().next()
}

#[cfg(test)]
pub(crate) fn sanitize_with(
    path: Option<&Path>,
    homes: &[&Path],
    canonicalize: &impl Fn(&Path) -> Option<PathBuf>,
) -> Result<SanitizedOriginPath, OriginSanitizeError> {
    sanitize_with_canonicalizers(path, homes, canonicalize, &|candidate| MaskPath {
        resolved: canonicalize(candidate),
        denied: false,
    })
}

struct MaskPath {
    resolved: Option<PathBuf>,
    denied: bool,
}

fn sanitize_with_canonicalizers(
    path: Option<&Path>,
    homes: &[&Path],
    canonicalize: &impl Fn(&Path) -> Option<PathBuf>,
    canonicalize_for_mask: &impl Fn(&Path) -> MaskPath,
) -> Result<SanitizedOriginPath, OriginSanitizeError> {
    let Some(path) = path else {
        return Ok(SanitizedOriginPath {
            kind: OriginPathKind::Unavailable,
            display: None,
        });
    };
    if path.as_os_str().is_empty() {
        return bounded(OriginPathKind::Absolute, String::new());
    }
    let spelling = path.to_string_lossy();
    if spelling.starts_with(r"\\.\pipe\") {
        return bounded(
            OriginPathKind::Absolute,
            escape_display_with_backslashes(&spelling),
        );
    }
    let parsed = DisplayPath::parse(path);
    // A terminal '..' has no reliable meaning when the preceding component
    // is missing or a symlink. Decline the label instead of guessing.
    if parsed.unresolved_parent {
        return bounded(OriginPathKind::Redacted, "<path unavailable>".to_owned());
    }
    // A relative display label has no authoritative filesystem root.
    // Neither the path nor any configured home may consult process cwd.
    if !parsed.absolute {
        return bounded(OriginPathKind::Absolute, escape_display(&parsed.render()));
    }
    let canonical_path = if parsed.absolute && !parsed.had_parent {
        canonicalize(path).map(|p| DisplayPath::parse(&p))
    } else {
        None
    };
    let mut best: Option<(usize, String)> = None;
    for home in homes
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, home)| {
            usable_home(home)
                && canonicalize(home)
                    .map(|resolved| !DisplayPath::parse(&resolved).is_multiuser_ancestor())
                    .unwrap_or(true)
                || (*index == 0
                    && homes
                        .get(1)
                        .is_some_and(|profile| redirected_home_root(home, profile)))
        })
        .map(|(_, home)| home)
    {
        // A redirected root may contain an own Users/<profile-name> path,
        // but it must not turn a different account into ~/Users/<name>/....
        let redirected_root = !usable_home(home);
        if redirected_root && parsed.mask_other_user().is_some() {
            let own_profile_account = homes.get(1).is_some_and(|profile| {
                let profile = DisplayPath::parse(profile);
                parsed.parts.get(1).zip(profile.parts.last()).is_some_and(
                    |(path_name, profile_name)| path_name.eq_ignore_ascii_case(profile_name),
                )
            });
            if !own_profile_account {
                continue;
            }
        }
        let home_parsed = DisplayPath::parse(home);
        let relative = parsed.relative_to(&home_parsed).or_else(|| {
            if !parsed.absolute || parsed.had_parent {
                return None;
            }
            let canonical_home = canonicalize(home).map(|p| DisplayPath::parse(&p))?;
            canonical_path.as_ref()?.relative_to(&canonical_home)
        });
        if let Some(mut relative) = relative {
            if redirected_root
                && relative
                    .first()
                    .is_some_and(|part| part.eq_ignore_ascii_case("Users"))
                && relative.len() > 1
            {
                relative[1] = "<user>".to_owned();
            }
            let rank = home_parsed.parts.len();
            if best.as_ref().is_none_or(|(previous, _)| rank > *previous) {
                let display = if relative.is_empty() {
                    "~".to_owned()
                } else {
                    format!("~/{}", relative.join("/"))
                };
                best = Some((rank, display));
            }
        }
    }
    if let Some((_, display)) = best {
        return bounded(OriginPathKind::HomeRelative, escape_display(&display));
    }
    let (masking_path, denied) = if let Some(canonical_path) = canonical_path {
        (Some(canonical_path), false)
    } else if parsed.absolute && !parsed.had_parent {
        let lookup = canonicalize_for_mask(path);
        (
            lookup
                .resolved
                .map(|resolved| DisplayPath::parse(&resolved)),
            lookup.denied,
        )
    } else {
        (None, false)
    };
    if let Some(masked) = parsed
        .mask_other_user()
        .or_else(|| masking_path.as_ref().and_then(DisplayPath::mask_other_user))
    {
        return bounded(OriginPathKind::Redacted, escape_display(&masked));
    }
    if denied {
        return bounded(OriginPathKind::Redacted, "<path unavailable>".to_owned());
    }
    bounded(OriginPathKind::Absolute, escape_display(&parsed.render()))
}

/// Only NotFound permits walking toward an existing ancestor. Permission,
/// ENOTDIR, EIO, and symlink loops decline canonical identity; lexical
/// masking still applies.
fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    canonicalize_existing_prefix_with(path, &Path::canonicalize)
}

/// A denied descendant may still have an accessible symlink/firmlink
/// ancestor. Use that ancestor only to find a maskable account root.
fn canonicalize_masking_prefix(path: &Path) -> MaskPath {
    canonicalize_masking_prefix_with(path, &Path::canonicalize)
}

fn canonicalize_masking_prefix_with(
    path: &Path,
    canonicalize: &impl Fn(&Path) -> std::io::Result<PathBuf>,
) -> MaskPath {
    if !path.is_absolute() {
        return MaskPath {
            resolved: None,
            denied: false,
        };
    }
    let mut existing = path;
    let mut unseen = Vec::new();
    let mut denied = false;
    loop {
        match canonicalize(existing) {
            Ok(mut canonical) => {
                canonical.extend(unseen.iter().rev());
                return MaskPath {
                    resolved: Some(canonical),
                    denied,
                };
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::PermissionDenied
                ) =>
            {
                denied |= error.kind() == ErrorKind::PermissionDenied;
                let Some(name) = existing.file_name() else {
                    return MaskPath {
                        resolved: None,
                        denied,
                    };
                };
                unseen.push(name);
                let Some(parent) = existing.parent() else {
                    return MaskPath {
                        resolved: None,
                        denied,
                    };
                };
                existing = parent;
            }
            Err(_) => {
                return MaskPath {
                    resolved: None,
                    denied,
                };
            }
        }
    }
}

fn canonicalize_existing_prefix_with(
    path: &Path,
    canonicalize: &impl Fn(&Path) -> std::io::Result<PathBuf>,
) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match canonicalize(existing) {
            Ok(mut canonical) => {
                canonical.extend(missing.iter().rev());
                return Some(canonical);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                missing.push(existing.file_name()?);
                existing = existing.parent()?;
            }
            Err(_) => return None,
        }
    }
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

#[derive(Debug)]
struct DisplayPath {
    root: String,
    parts: Vec<String>,
    absolute: bool,
    windows: bool,
    had_parent: bool,
    unresolved_parent: bool,
}

impl DisplayPath {
    fn is_wsl_share(&self) -> bool {
        self.root
            .strip_prefix("//")
            .and_then(|root| root.split('/').next())
            .is_some_and(|server| {
                server.eq_ignore_ascii_case("wsl$") || server.eq_ignore_ascii_case("wsl.localhost")
            })
    }

    fn is_multiuser_ancestor(&self) -> bool {
        let roots: &[&[&str]] = if self.root == "/" {
            &[
                &["Users"],
                &["home"],
                &["var", "home"],
                &["private", "var", "home"],
                &["System", "Volumes", "Data", "Users"],
                &["usr", "home"],
                &["export", "home"],
            ]
        } else {
            &[&["Users"], &["Documents and Settings"]]
        };
        let known = roots.iter().any(|root| {
            self.parts.len() <= root.len()
                && self.parts.iter().zip(*root).all(|(part, known)| {
                    (if self.windows {
                        part.trim_end_matches(['.', ' '])
                    } else {
                        part
                    })
                    .eq_ignore_ascii_case(known)
                })
        });
        known
            || (self.root == "/"
                && self
                    .parts
                    .first()
                    .is_some_and(|part| part.eq_ignore_ascii_case("mnt"))
                && self.parts.len() <= 3
                && (self.parts.len() == 1
                    || self.parts[1].len() == 1
                        && self.parts[1].as_bytes()[0].is_ascii_alphabetic()
                        && (self.parts.len() == 2 || self.parts[2].eq_ignore_ascii_case("Users"))))
            || (self.root == "/"
                && self
                    .parts
                    .first()
                    .is_some_and(|part| part.eq_ignore_ascii_case("Volumes"))
                && self.parts.len() <= 3
                && (self.parts.len() < 3 || self.parts[2].eq_ignore_ascii_case("Users")))
            || self.is_wsl_share()
                && self.parts.len() <= 1
                && self
                    .parts
                    .first()
                    .is_none_or(|part| part.eq_ignore_ascii_case("home"))
    }

    fn parse(path: &Path) -> Self {
        let original = path.to_string_lossy();
        let windows = cfg!(windows)
            || original.starts_with(r"\\")
            || original.starts_with(r"\??\")
            || original.starts_with("//")
            || (original.as_bytes().get(1) == Some(&b':')
                && matches!(original.as_bytes().get(2).copied(), Some(b'/' | b'\\')));
        let slash = if windows {
            original.replace('\\', "/")
        } else {
            original.to_string()
        };
        let verbatim = (cfg!(windows)
            || original.starts_with(r"\\?\")
            || original.starts_with(r"\\.\")
            || original.starts_with(r"\??\"))
        .then(|| {
            slash
                .strip_prefix("//?/")
                .or_else(|| slash.strip_prefix("//./"))
                .or_else(|| slash.strip_prefix("/??/"))
        })
        .flatten();
        let slash = verbatim
            .map(|tail| {
                if tail
                    .get(..4)
                    .is_some_and(|head| head.eq_ignore_ascii_case("UNC/"))
                {
                    format!("//{}", &tail[4..])
                } else if tail.is_empty() {
                    "/".to_owned()
                } else if tail.as_bytes().get(1) == Some(&b':') {
                    tail.to_owned()
                } else {
                    // Volume GUID and GLOBALROOT device paths are filesystem
                    // paths too. Keep their identity without the verbatim
                    // prefix so an embedded Users account can be masked.
                    format!("//device/{tail}")
                }
            })
            .unwrap_or(slash);
        let drive_root =
            slash.as_bytes().get(1) == Some(&b':') && slash.as_bytes().get(2) == Some(&b'/');
        let doubled_unix_root = (!cfg!(windows) && original.starts_with("///"))
            || !cfg!(windows)
                && !original.starts_with(r"\\")
                && slash.starts_with("//")
                && slash[2..].split('/').next().is_some_and(|head| {
                    ["Users", "home", "var", "private", "System"]
                        .iter()
                        .any(|root| head.eq_ignore_ascii_case(root))
                });
        let windows = windows
            && !doubled_unix_root
            && (cfg!(windows) || !(original.starts_with("//?/") || original.starts_with("//./")));
        let leading = if slash.starts_with("//") && !doubled_unix_root {
            "//"
        } else if slash.starts_with('/') {
            "/"
        } else {
            ""
        };
        let mut slash = format!(
            "{leading}{}",
            slash
                .trim_start_matches('/')
                .split('/')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("/")
        );
        if drive_root && slash.len() == 2 {
            slash.push('/');
        }
        let (root, tail, absolute) = if let Some(stripped) = slash.strip_prefix("//") {
            let mut components = stripped.split('/');
            let server = components.next().unwrap_or("");
            let share = components.next().unwrap_or("");
            if !server.is_empty() && !share.is_empty() {
                let prefix = format!("//{server}/{share}");
                let remainder = &slash[prefix.len()..];
                (prefix, remainder, true)
            } else {
                ("/".to_owned(), slash.trim_start_matches('/'), true)
            }
        } else if slash.as_bytes().get(1) == Some(&b':') && slash.as_bytes().get(2) == Some(&b'/') {
            (slash[..2].to_owned(), &slash[3..], true)
        } else if slash.starts_with('/') {
            ("/".to_owned(), slash.trim_start_matches('/'), true)
        } else {
            (String::new(), slash.as_str(), false)
        };
        let mut parts: Vec<String> = Vec::new();
        let mut had_parent = false;
        let mut unresolved_parent = false;
        for part in tail
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            if part == ".." {
                had_parent = true;
                if !absolute {
                    parts.push("..".to_owned());
                } else if let Some(last) = parts.last()
                    && last != ".."
                {
                    parts.pop();
                } else {
                    unresolved_parent = true;
                }
            } else {
                parts.push(part.to_owned());
            }
        }
        // A trailing parent may depend on a missing or symlinked last
        // component. Decline it rather than treating that as the home.
        if absolute && tail.trim_end_matches('/').ends_with("/..") {
            unresolved_parent = true;
        }
        Self {
            root,
            parts,
            absolute,
            windows,
            had_parent,
            unresolved_parent,
        }
    }

    fn relative_to(&self, home: &Self) -> Option<Vec<String>> {
        if !self.absolute || !home.absolute || self.parts.len() < home.parts.len() {
            return None;
        }
        let equal = |a: &str, b: &str| {
            if self.windows || home.windows {
                a.trim_end_matches(['.', ' '])
                    .eq_ignore_ascii_case(b.trim_end_matches(['.', ' ']))
            } else {
                a == b
            }
        };
        if !equal(&self.root, &home.root)
            || !self.parts.iter().zip(&home.parts).all(|(a, b)| equal(a, b))
        {
            return None;
        }
        Some(self.parts[home.parts.len()..].to_vec())
    }

    fn render(&self) -> String {
        if self.root == "/" {
            return format!("/{}", self.parts.join("/"));
        }
        if self.root.starts_with("//") {
            return if self.parts.is_empty() {
                self.root.clone()
            } else {
                format!("{}/{}", self.root, self.parts.join("/"))
            };
        }
        if self.root.ends_with(':') {
            return format!(
                "{}{}",
                self.root,
                if self.parts.is_empty() {
                    "/".to_owned()
                } else {
                    format!("/{}", self.parts.join("/"))
                }
            );
        }
        if self.parts.is_empty() {
            return if self.absolute {
                "/".to_owned()
            } else {
                ".".to_owned()
            };
        }
        self.parts.join("/")
    }

    fn mask_other_user(&self) -> Option<String> {
        if !self.absolute {
            return None;
        }
        if !self.windows && (self.root.starts_with("//?/") || self.root.starts_with("//./")) {
            return None;
        }
        let lower = |part: &str, value: &str| {
            if self.windows {
                part.trim_end_matches(['.', ' '])
                    .eq_ignore_ascii_case(value)
            } else {
                part.eq_ignore_ascii_case(value)
            }
        };
        let index = if self.root == "/" {
            if self
                .parts
                .first()
                .is_some_and(|p| lower(p, "Users") || lower(p, "home"))
            {
                Some(1)
            } else if self.parts.len() >= 2
                && lower(&self.parts[0], "var")
                && lower(&self.parts[1], "home")
            {
                Some(2)
            } else if self.parts.len() >= 3
                && lower(&self.parts[0], "private")
                && lower(&self.parts[1], "var")
                && lower(&self.parts[2], "home")
            {
                Some(3)
            } else if self.parts.len() >= 4
                && lower(&self.parts[0], "System")
                && lower(&self.parts[1], "Volumes")
                && lower(&self.parts[2], "Data")
                && lower(&self.parts[3], "Users")
            {
                Some(4)
            } else if self.parts.len() >= 4
                && lower(&self.parts[2], "Users")
                && (lower(&self.parts[0], "Volumes")
                    || lower(&self.parts[0], "mnt")
                        && self.parts[1].len() == 1
                        && self.parts[1].as_bytes()[0].is_ascii_alphabetic())
            {
                Some(3)
            } else if self.parts.len() >= 3
                && (lower(&self.parts[0], "usr") || lower(&self.parts[0], "export"))
                && lower(&self.parts[1], "home")
            {
                Some(2)
            } else {
                None
            }
        } else if self.root.starts_with("//device/") {
            self.parts
                .iter()
                .position(|part| lower(part, "Users"))
                .map(|index| index + 1)
        } else if self.root.ends_with(':') || self.root.starts_with("//") {
            if self.parts.first().is_some_and(|p| {
                lower(p, "Users") || self.root.ends_with(':') && lower(p, "Documents and Settings")
            }) {
                Some(1)
            } else if self.root.starts_with("//")
                && self
                    .root
                    .rsplit('/')
                    .next()
                    .is_some_and(|share| lower(share, "Users"))
            {
                Some(0)
            } else if self.is_wsl_share() {
                self.parts
                    .first()
                    .is_some_and(|part| lower(part, "home"))
                    .then_some(1)
            } else if self.root.ends_with(':')
                && self.parts.len() >= 3
                && lower(&self.parts[0], "msys64")
                && lower(&self.parts[1], "home")
            {
                Some(2)
            } else {
                None
            }
        } else {
            None
        }?;
        if self.parts.len() <= index {
            return None;
        } // bare root has no identity
        let mut masked = self.parts.clone();
        masked[index] = "<user>".to_owned();
        // Rebuild from components, never substring replacement (the same
        // spelling can occur elsewhere in the path).
        let separator = if self.root == "/" { "" } else { "/" };
        Some(format!("{}{separator}{}", self.root, masked.join("/")))
    }
}

fn escape_display(text: &str) -> String {
    escape_display_with(text, false)
}

fn escape_display_with_backslashes(text: &str) -> String {
    escape_display_with(text, true)
}

fn escape_display_with(text: &str, preserve_backslashes: bool) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        let code = character as u32;
        let is_bidi = matches!(code, 0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069);
        if character.is_control() || is_bidi || (!preserve_backslashes && character == '\\') {
            use std::fmt::Write;
            let _ = write!(escaped, "\\u{{{code:04x}}}");
        } else {
            escaped.push(character);
        }
    }
    escaped
}
