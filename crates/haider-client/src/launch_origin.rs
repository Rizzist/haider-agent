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

/// Whether a workspace root can contain distinct account directories.
/// Tool rows must not shorten a path relative to such a root: doing so
/// would remove the account mask from the displayed label.
#[must_use]
pub fn is_multiuser_workspace_root(root: &Path) -> bool {
    DisplayPath::parse(root).is_multiuser_ancestor()
        || root
            .canonicalize()
            .ok()
            .is_some_and(|resolved| DisplayPath::parse(&resolved).is_multiuser_ancestor())
}

/// A display-only workspace label, using the same path components and
/// Windows spelling rules as home and account projection. Never use this
/// label for filesystem access. Dot segments can cross a workspace boundary
/// (or a symlink), so they make the label unavailable even when lexical
/// normalization would place the final path below the root.
#[must_use]
pub fn workspace_relative_display(path: &Path, root: &Path) -> Option<String> {
    let parsed_path = DisplayPath::parse(path);
    let parsed_root = DisplayPath::parse(root);
    let windows = parsed_path.windows || parsed_root.windows;
    let has_dot_segment = |path: &Path| {
        let spelling = path.to_string_lossy();
        // The dot in a Win32 device namespace identifies the namespace,
        // not a traversal component. Only inspect its filesystem tail.
        let tail = spelling
            .strip_prefix(r"\\.\")
            .or_else(|| windows.then(|| spelling.strip_prefix("//./")).flatten())
            .unwrap_or(&spelling);
        tail.split(['/', '\\']).any(|component| {
            matches!(component, "." | "..")
                    // Win32 normally strips trailing ASCII spaces and dots
                    // from components. A dot-only spelling can therefore be
                    // an alias for a current/parent segment. Verbatim paths
                    // get the same conservative display rule.
                    || windows
                        && !component.is_empty()
                        && component.chars().all(|ch| ch == '.' || ch == ' ')
        })
    };
    if has_dot_segment(path) || has_dot_segment(root) {
        return None;
    }
    if !parsed_path.absolute || !parsed_root.absolute {
        return None;
    }
    // Shortening must remove the recognized account component rather than
    // expose it in the remainder. Compare the identities that matched: a
    // canonical workspace alias can have a different component depth.
    let exposes_account = |path: &DisplayPath, root: &DisplayPath| {
        path.account_index()
            .filter(|index| *index < path.parts.len())
            .is_some_and(|index| index >= root.parts.len())
    };
    let relative = if let Some(relative) = parsed_path.relative_to(&parsed_root) {
        // A known unsafe lexical match needs no filesystem lookup.
        if exposes_account(&parsed_path, &parsed_root) {
            return None;
        }
        // An interior symlink can leave a narrow workspace even though its
        // requested spelling remains below that root. Keep the lexical label
        // only when the resolved identity is safe to shorten as well.
        let lookup = canonicalize_masking_prefix(path);
        if lookup.denied {
            return None;
        }
        if let Some(resolved) = lookup.resolved {
            let resolved_path = DisplayPath::parse(&resolved);
            let resolved_root = canonicalize_existing_prefix(root)
                .map(|root| DisplayPath::parse(&root))
                .unwrap_or_else(|| DisplayPath::parse(root));
            if resolved_path
                .canonical_relative_to(&resolved_root)
                .is_none()
                || exposes_account(&resolved_path, &resolved_root)
            {
                return None;
            }
        }
        relative
    } else {
        // Canonical spellings cover host aliases such as /tmp -> /private/tmp.
        // A failure to resolve either side keeps the original projected label.
        let path = DisplayPath::parse(&canonicalize_existing_prefix(path)?);
        let root = DisplayPath::parse(&canonicalize_existing_prefix(root)?);
        let relative = path.canonical_relative_to(&root)?;
        if exposes_account(&path, &root) {
            return None;
        }
        relative
    };
    let label = if relative.is_empty() {
        ".".to_owned()
    } else {
        relative.join("/")
    };
    Some(display_path_with_homes(Path::new(&label), &[]))
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
        && home.account_index() != Some(0)
        && !home.is_device()
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
    let pipe_prefix = r"\\.\pipe\";
    if spelling
        .get(..pipe_prefix.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(pipe_prefix))
    {
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
    let canonical_path = if parsed.absolute {
        canonicalize(path).map(|p| DisplayPath::parse(&p))
    } else {
        None
    };
    let lookup = if canonical_path.is_none() && parsed.absolute {
        Some(canonicalize_for_mask(path))
    } else {
        None
    };
    let fallback_path = lookup
        .as_ref()
        .and_then(|lookup| lookup.resolved.as_ref())
        .map(|path| DisplayPath::parse(path));
    let masking_path = canonical_path.as_ref().or(fallback_path.as_ref());
    let denied = lookup.as_ref().is_some_and(|lookup| lookup.denied);
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
        let home_parsed = DisplayPath::parse(home);
        let own_profile_account = |source: &DisplayPath, index: usize| {
            homes.get(1).is_some_and(|profile| {
                let canonical_profile = canonicalize(profile);
                let profile = DisplayPath::parse(canonical_profile.as_deref().unwrap_or(profile));
                let profile_name = profile
                    .account_index()
                    .and_then(|index| profile.parts.get(index))
                    .or_else(|| profile.parts.last());
                source.parts.get(index).zip(profile_name).is_some_and(
                    |(path_name, profile_name)| {
                        if source.is_wsl_share() || profile.is_wsl_share() {
                            path_name == profile_name
                        } else if masking_path.is_some() || canonical_profile.is_some() {
                            if source.canonical_account_identity(
                                index,
                                &profile,
                                &directory_identity,
                            ) == Some(false)
                            {
                                return false;
                            }
                            if source.windows || profile.windows {
                                path_name.eq_ignore_ascii_case(profile_name)
                            } else {
                                path_name == profile_name
                            }
                        } else {
                            source.component_eq(path_name, profile_name)
                        }
                    },
                )
            })
        };
        let relative = parsed
            .relative_to(&home_parsed)
            .and_then(|relative| {
                if denied {
                    return None;
                }
                let Some(source) = masking_path else {
                    return Some((relative, &parsed, home_parsed.parts.len()));
                };
                let resolved_home = canonicalize(home)
                    .map(|home| DisplayPath::parse(&home))
                    .unwrap_or_else(|| DisplayPath::parse(home));
                let resolved_relative = source.canonical_relative_to(&resolved_home)?;
                // A redirected drive may resolve to an account namespace at
                // a different depth. Validate and mask that actual remainder.
                if source
                    .account_index()
                    .filter(|index| *index < source.parts.len())
                    .is_some_and(|index| index >= resolved_home.parts.len())
                {
                    return Some((resolved_relative, source, resolved_home.parts.len()));
                }
                Some((relative, &parsed, home_parsed.parts.len()))
            })
            .or_else(|| {
                if !parsed.absolute {
                    return None;
                }
                let canonical_home = canonicalize(home).map(|p| DisplayPath::parse(&p))?;
                let source = canonical_path.as_ref()?;
                Some((
                    source.canonical_relative_to(&canonical_home)?,
                    source,
                    canonical_home.parts.len(),
                ))
            });
        if let Some((mut relative, source, home_depth)) = relative {
            if let Some(index) = source
                .account_index()
                .filter(|index| *index < source.parts.len())
                && index >= home_depth
            {
                if !redirected_root || !own_profile_account(source, index) {
                    continue;
                }
                relative[index - home_depth] = "<user>".to_owned();
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
    // Keep harmless requested case spelling when resolution only normalized
    // case or the macOS /private prefix. A changed identity (including '..') must
    // supply the label, since the lexical remainder can contain its account.
    let mask_source = masking_path.filter(|source| {
        let source_display = source.render();
        let requested_display = parsed.render();
        let equivalent = source_display.eq_ignore_ascii_case(&requested_display)
            || (cfg!(target_os = "macos")
                && source_display
                    .strip_prefix("/private")
                    .is_some_and(|path| path.eq_ignore_ascii_case(&requested_display)));
        parsed.had_parent || !equivalent
    });
    if let Some(masked) = mask_source
        .and_then(DisplayPath::mask_other_user)
        .or_else(|| parsed.mask_other_user())
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
    canonicalize_existing_prefix_with(path, &Path::canonicalize, cfg!(windows))
}

/// A denied descendant may still have an accessible symlink/firmlink
/// ancestor. Use that ancestor only to find a maskable account root.
fn canonicalize_masking_prefix(path: &Path) -> MaskPath {
    canonicalize_masking_prefix_with(path, &Path::canonicalize, cfg!(windows))
}

// An unseen Win32 dot/space parent alias cannot be appended to an existing
// prefix: its identity depends on the directory preceding the traversal.
fn missing_parent_component(name: &std::ffi::OsStr, windows: bool) -> bool {
    let spelling = name.to_string_lossy();
    windows
        && spelling.chars().all(|ch| ch == '.' || ch == ' ')
        && spelling.chars().filter(|ch| *ch == '.').count() >= 2
}

// The unresolved suffix still has the naming rules of the REQUESTED path.
// In an ordinary Win32 path, a missing `alice.` is an alias for `alice`;
// appending it verbatim would invent a conflicting canonical account name.
// Namespace spellings retain their literal suffix, as do POSIX filenames.
fn append_missing_path(
    requested: &Path,
    mut canonical: PathBuf,
    missing: &[&std::ffi::OsStr],
    windows: bool,
) -> Option<PathBuf> {
    let win32_names = windows && {
        let spelling = requested.to_string_lossy().replace('\\', "/");
        !["//?/", "//./", "/??/"]
            .iter()
            .any(|prefix| spelling.starts_with(prefix))
    };
    for name in missing.iter().rev() {
        if win32_names && let Some(spelling) = name.to_str() {
            let spelling = spelling.trim_end_matches(['.', ' ']);
            if spelling.is_empty() {
                return None;
            }
            canonical.push(spelling);
        } else {
            canonical.push(name);
        }
    }
    Some(canonical)
}

fn canonicalize_masking_prefix_with(
    path: &Path,
    canonicalize: &impl Fn(&Path) -> std::io::Result<PathBuf>,
    windows: bool,
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
            Ok(canonical) => {
                return MaskPath {
                    resolved: append_missing_path(path, canonical, &unseen, windows),
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
                if missing_parent_component(name, windows) {
                    return MaskPath {
                        resolved: None,
                        denied,
                    };
                }
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
    windows: bool,
) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match canonicalize(existing) {
            Ok(canonical) => {
                return append_missing_path(path, canonical, &missing, windows);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let name = existing.file_name()?;
                if missing_parent_component(name, windows) {
                    return None;
                }
                missing.push(name);
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
    raw: PathBuf,
    root: String,
    parts: Vec<String>,
    absolute: bool,
    windows: bool,
    had_parent: bool,
    unresolved_parent: bool,
}

fn directory_identity(left: &Path, right: &Path) -> Option<bool> {
    if !left.is_absolute() || !right.is_absolute() {
        return None;
    }
    if left.metadata().is_ok_and(|metadata| !metadata.is_dir())
        || right.metadata().is_ok_and(|metadata| !metadata.is_dir())
    {
        return Some(false);
    }
    match same_file::is_same_file(left, right) {
        Ok(same) => Some(same),
        Err(_)
            if matches!(left.try_exists(), Ok(false))
                && matches!(right.try_exists(), Ok(false)) =>
        {
            None
        }
        Err(_) => Some(false),
    }
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

    fn component_eq(&self, part: &str, known: &str) -> bool {
        if self.windows {
            part.trim_end_matches(['.', ' '])
                .eq_ignore_ascii_case(known.trim_end_matches(['.', ' ']))
        } else {
            part.eq_ignore_ascii_case(known)
        }
    }

    fn is_device(&self) -> bool {
        self.root
            .get(..9)
            .is_some_and(|root| root.eq_ignore_ascii_case("//device/"))
    }

    /// Account-parent components, shared by masking and ancestor validation.
    /// Device paths can contain account parents at any depth; retain that offset.
    fn account_roots(&self) -> Vec<Vec<&str>> {
        if self.is_device() {
            for (index, part) in self.parts.iter().enumerate() {
                if self.component_eq(part, "Users")
                    || self.component_eq(part, "Documents and Settings")
                {
                    return vec![self.parts[..=index].iter().map(String::as_str).collect()];
                }
                if self.component_eq(part, "msys64")
                    && self
                        .parts
                        .get(index + 1)
                        .is_none_or(|next| self.component_eq(next, "home"))
                {
                    let mut root: Vec<_> =
                        self.parts[..=index].iter().map(String::as_str).collect();
                    root.push("home");
                    return vec![root];
                }
            }
            if self
                .root
                .rsplit('/')
                .next()
                .is_some_and(|part| self.component_eq(part, "GLOBALROOT"))
            {
                let volume = self.parts.get(1).map_or("", String::as_str);
                return vec![vec!["Device", volume, "Users"]];
            }
            return vec![vec!["Users"]];
        }
        if self.root == "/" {
            let mut roots = vec![
                vec!["Users"],
                vec!["home"],
                vec!["var", "home"],
                vec!["private", "var", "home"],
                vec!["System", "Volumes", "Data", "Users"],
                vec!["usr", "home"],
                vec!["export", "home"],
            ];
            if self
                .parts
                .first()
                .is_some_and(|part| self.component_eq(part, "Volumes"))
            {
                roots.push(vec![
                    "Volumes",
                    self.parts.get(1).map_or("", String::as_str),
                    "Users",
                ]);
            }
            if self
                .parts
                .first()
                .is_some_and(|part| self.component_eq(part, "mnt"))
                && self.parts.get(1).is_none_or(|drive| {
                    drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic()
                })
            {
                roots.push(vec![
                    "mnt",
                    self.parts.get(1).map_or("", String::as_str),
                    "Users",
                ]);
            }
            return roots;
        }
        if self.root.starts_with("//")
            && self.root.rsplit('/').next().is_some_and(|share| {
                self.component_eq(share, "Users")
                    || self.component_eq(share, "Documents and Settings")
            })
        {
            return vec![vec![]];
        }
        let mut roots = vec![
            vec!["Users"],
            vec!["Documents and Settings"],
            vec!["msys64", "home"],
        ];
        if self.is_wsl_share()
            || self.root.starts_with("//")
                && self
                    .root
                    .rsplit('/')
                    .next()
                    .is_some_and(|share| self.component_eq(share, "msys64"))
        {
            roots.push(vec!["home"]);
        }
        roots
    }

    fn is_multiuser_ancestor(&self) -> bool {
        self.absolute
            && self.account_roots().iter().any(|root| {
                self.parts.len() <= root.len()
                    && self
                        .parts
                        .iter()
                        .zip(root)
                        .all(|(part, known)| self.component_eq(part, known))
            })
    }

    /// Index of the account, including a bare parent with no account yet.
    fn account_index(&self) -> Option<usize> {
        if !self.absolute
            || !self.windows && (self.root.starts_with("//?/") || self.root.starts_with("//./"))
        {
            return None;
        }
        self.account_roots().iter().find_map(|root| {
            (self.parts.len() >= root.len()
                && self
                    .parts
                    .iter()
                    .zip(root)
                    .all(|(part, known)| self.component_eq(part, known)))
            .then_some(root.len())
        })
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
        for part in tail.split('/').filter(|part| !part.is_empty()) {
            // Win32's ordinary path form removes trailing ASCII dots and
            // spaces. A component made only of them may therefore act as a
            // current or parent segment. Treat verbatim spellings the same
            // way for conservative display masking; this is never used for IO.
            let dot_count = (absolute && windows && part.chars().all(|ch| ch == '.' || ch == ' '))
                .then(|| part.bytes().filter(|byte| *byte == b'.').count());
            if part == "." || dot_count.is_some_and(|count| count <= 1) {
                continue;
            }
            if part == ".." || dot_count.is_some_and(|count| count >= 2) {
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
        let last = tail.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let trailing_dot_count = (windows && last.chars().all(|ch| ch == '.' || ch == ' '))
            .then(|| last.bytes().filter(|byte| *byte == b'.').count());
        if absolute && (last == ".." || trailing_dot_count.is_some_and(|count| count >= 2)) {
            unresolved_parent = true;
        }
        Self {
            raw: path.to_owned(),
            root,
            parts,
            absolute,
            windows,
            had_parent,
            unresolved_parent,
        }
    }

    fn relative_to(&self, home: &Self) -> Option<Vec<String>> {
        self.relative_to_with(home, true)
    }

    // Canonical names are actual directory names: an extended Windows path
    // can identify a literal trailing dot or space, rather than a Win32 alias.
    fn canonical_relative_to(&self, home: &Self) -> Option<Vec<String>> {
        self.canonical_relative_to_with_identity(home, &directory_identity)
    }

    fn canonical_relative_to_with_identity(
        &self,
        home: &Self,
        identity: &impl Fn(&Path, &Path) -> Option<bool>,
    ) -> Option<Vec<String>> {
        let relative = self.relative_to_with(home, false)?;
        // Windows directories can opt into case-sensitive names. A folded
        // prefix alone cannot establish containment when both names exist.
        // Query the original paths, never a reconstructed display label.
        let prefix = self.raw.ancestors().nth(relative.len())?;
        if prefix != home.raw && identity(prefix, &home.raw) == Some(false) {
            return None;
        }
        Some(relative)
    }

    fn canonical_account_identity(
        &self,
        index: usize,
        profile: &Self,
        identity: &impl Fn(&Path, &Path) -> Option<bool>,
    ) -> Option<bool> {
        let profile_index = profile
            .account_index()
            .or_else(|| profile.parts.len().checked_sub(1))?;
        let account = self
            .raw
            .ancestors()
            .nth(self.parts.len().checked_sub(index + 1)?)?;
        let profile_account = profile
            .raw
            .ancestors()
            .nth(profile.parts.len().checked_sub(profile_index + 1)?)?;
        // Different redirected roots retain their profile-name policy. Only
        // equivalent canonical prefixes can denote a case-sensitive sibling.
        let same_prefix =
            Self::parse(account).relative_to_with(&Self::parse(profile_account), false)?;
        if !same_prefix.is_empty() || account == profile_account {
            return None;
        }
        identity(account, profile_account)
    }

    fn relative_to_with(&self, home: &Self, trim_win32: bool) -> Option<Vec<String>> {
        if !self.absolute || !home.absolute || self.parts.len() < home.parts.len() {
            return None;
        }
        let equal = |a: &str, b: &str| {
            if self.windows || home.windows {
                if trim_win32 {
                    a.trim_end_matches(['.', ' '])
                        .eq_ignore_ascii_case(b.trim_end_matches(['.', ' ']))
                } else {
                    a.eq_ignore_ascii_case(b)
                }
            } else {
                a == b
            }
        };
        if !equal(&self.root, &home.root)
            || !self.parts.iter().zip(&home.parts).all(|(a, b)| {
                if self.is_wsl_share() || home.is_wsl_share() {
                    a == b
                } else {
                    equal(a, b)
                }
            })
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
        let index = self.account_index()?;
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
