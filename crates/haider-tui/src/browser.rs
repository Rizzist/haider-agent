//! The runtime's browser hop — the ONE place haider launches an external
//! program on the user's behalf (the OAuth authorize URL).
//!
//! Two laws:
//! - the scheme is allow-listed to `http`/`https` BEFORE anything spawns,
//!   so a hostile "authorize URL" can never become an arbitrary-program
//!   launch (`open` on macOS happily executes `file:` and `ssh:` handlers);
//! - `$BROWSER` wins when set (the POSIX convention), which is also what
//!   makes the hop probeable: a PTY probe points `$BROWSER` at a recorder
//!   script and asserts the REAL authorize URL arrived.

use std::path::Path;
use std::process::{Command, Stdio};

/// Build the platform's opener command for `url` without spawning it.
///
/// Errors on a scheme outside http/https — the caller treats that exactly
/// like a failed spawn (honest fallback, never a launch).
pub fn open_url_command(url: &str) -> std::io::Result<Command> {
    open_url_command_with_env(url, &|name| std::env::var(name).ok())
}

/// The env-injected body of [`open_url_command`] — tests hand in a fake
/// environment instead of mutating the process's.
pub fn open_url_command_with_env(
    url: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> std::io::Result<Command> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to open a non-http(s) URL",
        ));
    }
    let mut command = if let Some(browser) = env("BROWSER").filter(|value| !value.trim().is_empty())
    {
        let mut command = Command::new(browser);
        command.arg(url);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("/usr/bin/open");
        command.arg(url);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        // `start`'s first quoted argument is the window TITLE — the empty
        // string keeps the URL out of that slot.
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

/// Open `url` in the user's browser, detached — the TUI never waits on it
/// and never inherits its stdio (the terminal is ours).
pub fn open_url(url: &str) -> std::io::Result<()> {
    open_url_command(url)?.spawn().map(drop)
}

/// Build the platform file-explorer command for an existing payload path.
///
/// Command construction is kept pure (apart from validating filesystem
/// existence), allowing the exact program and argument boundary to be pinned
/// without launching an OS application in tests.
/// Reveal round 2 — the TRUST BOUNDARY. The path arrives from a durable
/// event an old or foreign daemon could have authored, so nothing about it
/// is trusted: it must already be absolute (a relative token could parse as
/// an OPTION to the opener), never a Windows UNC/device form (`metadata`
/// alone would touch the network before Explorer even starts), and after
/// canonicalization it must be a REGULAR FILE with an image extension —
/// this surface reveals images the harness reported, nothing else.
fn validated_reveal_target(path: &Path) -> std::io::Result<std::path::PathBuf> {
    let refused =
        |message: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, message.to_owned());
    if !path.is_absolute() {
        return Err(refused("reveal requires an absolute path"));
    }
    let raw = path.as_os_str().to_string_lossy();
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        return Err(refused("reveal refuses UNC/device paths"));
    }
    const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg"];
    let extension_ok = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        });
    if !extension_ok {
        return Err(refused("reveal is scoped to image files"));
    }
    let canonical = path.canonicalize()?;
    if !std::fs::metadata(&canonical)?.is_file() {
        return Err(refused("reveal target is not a regular file"));
    }
    Ok(canonical)
}

pub fn reveal_path_command(path: &Path) -> std::io::Result<Command> {
    let path = validated_reveal_target(path)?;
    let path = path.as_path();
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("/usr/bin/open");
        command.arg("-R").arg(path);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("explorer");
        command.arg(format!("/select,{}", path.display()));
        command
    } else {
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "image path has no parent directory",
            )
        })?;
        let mut command = Command::new("xdg-open");
        command.arg(parent);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

/// Reveal `path` in the platform file explorer, detached from the TUI.
pub fn reveal_path(path: &str) -> std::io::Result<()> {
    reveal_path_command(Path::new(path))?.spawn().map(drop)
}
