//! whisper-cli runtime discovery and install drivers (ADE parity).
//!
//! Discovery order (rust-diffforge `src-tauri/src/audio.rs:3769-3876`):
//! managed `<whisper_dir>/runtime/` (recursive, per executable name) → PATH →
//! well-known absolute paths. Executable names probed in order:
//! `whisper-cli`, `main`, `whisper` (`.exe` on Windows). One runtime serves
//! both products: Haider never installs a private copy when the ADE already
//! installed one anywhere on this list.
//!
//! Install drivers:
//! - macOS: drive `brew install whisper-cpp` (ADE
//!   `install_whisper_runtime_with_homebrew`); a missing Homebrew is an
//!   honest hint, not an error dump.
//! - Windows: the pinned official v1.8.4 zip + sha256, downloaded with the
//!   crate's verify-before-rename machinery, entry names screened against
//!   zip-slip, then extracted in-process into `<whisper_dir>/runtime/`.
//! - Linux: no managed download (ADE `WHISPER_RUNTIME_URL = None`) — a
//!   PATH-only hint.

use std::ffi::OsStr;
use std::io::{Read, Seek, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::SttError;
use crate::download::{DOWNLOAD_TIMEOUT_SECS, DownloadProgress, DownloadSpec, install};

/// Official pinned Windows runtime package (ADE `WHISPER_RUNTIME_URL`).
pub const WINDOWS_RUNTIME_ZIP_URL: &str =
    "https://github.com/ggml-org/whisper.cpp/releases/download/v1.8.4/whisper-bin-x64.zip";
/// Pinned sha256 of the Windows runtime zip (ADE `WHISPER_RUNTIME_SHA256`).
pub const WINDOWS_RUNTIME_ZIP_SHA256: &str =
    "74f973345cb52ef5ba3ec9e7e7af8e48cc8c71722d1528603b80588a11f82e3e";
/// Zip filename inside the whisper dir (ADE `WHISPER_RUNTIME_ZIP_FILE`).
pub const WINDOWS_RUNTIME_ZIP_FILE: &str = "whisper-bin-x64.zip";

/// Per-OS "how do I get whisper-cli" hint (ADE `WHISPER_RUNTIME_INSTALL_HINT`).
#[cfg(target_os = "macos")]
pub const RUNTIME_INSTALL_HINT: &str = "Install whisper.cpp CLI with Homebrew: brew install whisper-cpp. If Homebrew is missing, install it from https://brew.sh, then recheck.";
#[cfg(target_os = "macos")]
pub const HOMEBREW_MISSING_HINT: &str = "Homebrew is required to install whisper.cpp automatically. Install Homebrew from https://brew.sh, then recheck.";
#[cfg(all(unix, not(target_os = "macos")))]
pub const RUNTIME_INSTALL_HINT: &str =
    "Install whisper.cpp CLI and make whisper-cli, whisper, or main available on PATH.";
#[cfg(windows)]
pub const RUNTIME_INSTALL_HINT: &str =
    "Haider can download the official whisper.cpp x64 runtime automatically.";

/// Executable names probed, in order (ADE `whisper_runtime_executable_names`).
#[must_use]
pub fn runtime_executable_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["whisper-cli.exe", "main.exe", "whisper.exe"]
    }
    #[cfg(not(windows))]
    {
        &["whisper-cli", "main", "whisper"]
    }
}

/// Well-known absolute runtime paths (ADE `common_whisper_runtime_paths`).
#[must_use]
pub fn well_known_runtime_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/opt/homebrew/bin/whisper-cli"),
            PathBuf::from("/usr/local/bin/whisper-cli"),
            PathBuf::from("/opt/homebrew/bin/whisper"),
            PathBuf::from("/usr/local/bin/whisper"),
            PathBuf::from("/opt/homebrew/bin/main"),
            PathBuf::from("/usr/local/bin/main"),
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![
            PathBuf::from("/usr/local/bin/whisper-cli"),
            PathBuf::from("/usr/bin/whisper-cli"),
            PathBuf::from("/usr/local/bin/main"),
            PathBuf::from("/usr/bin/main"),
        ]
    }
    #[cfg(windows)]
    {
        Vec::new()
    }
}

/// The managed runtime directory inside the shared whisper dir.
#[must_use]
pub fn runtime_directory(whisper_dir: &Path) -> PathBuf {
    whisper_dir.join("runtime")
}

fn find_named_in_dir(directory: &Path, runtime_name: &str) -> Option<PathBuf> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
            if runtime_name.eq_ignore_ascii_case(name) {
                return Some(path);
            }
        }
    }
    None
}

/// Finds a runtime executable anywhere under `directory` (recursive), probing
/// each canonical name in order.
#[must_use]
pub fn find_runtime_in_dir(directory: &Path) -> Option<PathBuf> {
    runtime_executable_names()
        .iter()
        .find_map(|name| find_named_in_dir(directory, name))
}

/// Finds the first canonical executable across `path_value` entries (an
/// injected `$PATH` snapshot, so discovery is deterministic under test).
#[must_use]
pub fn find_on_path(names: &[&str], path_value: Option<&OsStr>) -> Option<PathBuf> {
    let path_value = path_value?;
    for directory in std::env::split_paths(path_value) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Full discovery with every input injected: managed dir → PATH →
/// well-known list. Returns the first hit.
#[must_use]
pub fn discover_runtime_with(
    whisper_dir: &Path,
    path_value: Option<&OsStr>,
    well_known: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(runtime) = find_runtime_in_dir(&runtime_directory(whisper_dir)) {
        return Some(runtime);
    }
    if let Some(runtime) = find_on_path(runtime_executable_names(), path_value) {
        return Some(runtime);
    }
    well_known
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
}

/// Production discovery: process `$PATH` and the platform well-known list.
#[must_use]
pub fn discover_runtime(whisper_dir: &Path) -> Option<PathBuf> {
    discover_runtime_with(
        whisper_dir,
        std::env::var_os("PATH").as_deref(),
        &well_known_runtime_paths(),
    )
}

/// Locates Homebrew (ADE `homebrew_executable_path`): PATH, then the two
/// canonical install locations.
#[must_use]
pub fn homebrew_path_with(path_value: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(brew) = find_on_path(&["brew"], path_value) {
        return Some(brew);
    }
    [
        PathBuf::from("/opt/homebrew/bin/brew"),
        PathBuf::from("/usr/local/bin/brew"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

/// Outcome of one install-driver attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeInstallOutcome {
    /// The driver ran to completion; re-discover to locate the binary.
    Installed,
    /// The driver cannot run here; carries the honest hint to show.
    Unavailable { hint: String },
}

fn first_output_line(stdout: &[u8], stderr: &[u8]) -> String {
    let text = if stderr.is_empty() { stdout } else { stderr };
    String::from_utf8_lossy(text)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Drives `brew install whisper-cpp` through an explicit brew executable.
///
/// Missing-brew handling lives in [`install_runtime`]; this driver assumes
/// `brew_path` exists and maps a non-zero exit to a typed error carrying the
/// first non-empty output line (never the full log).
pub async fn install_runtime_with_homebrew(brew_path: &Path) -> Result<(), SttError> {
    let output = tokio::time::timeout(
        Duration::from_secs(DOWNLOAD_TIMEOUT_SECS),
        tokio::process::Command::new(brew_path)
            .args(["install", "whisper-cpp"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| SttError::Timeout("Homebrew install did not finish".into()))?
    .map_err(|error| SttError::Io(format!("could not run Homebrew: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = first_output_line(&output.stdout, &output.stderr);
    if detail.is_empty() {
        return Err(SttError::Endpoint(
            "Homebrew could not install whisper.cpp.".into(),
        ));
    }
    Err(SttError::Endpoint(format!(
        "Homebrew could not install whisper.cpp: {detail}"
    )))
}

/// Zip-slip guard: refuses any archive entry that could escape the
/// extraction directory (absolute paths, drive prefixes, `..` components).
pub fn reject_unsafe_zip_entries(entry_names: &str) -> Result<(), SttError> {
    for entry in entry_names.lines().map(str::trim) {
        if entry.is_empty() {
            continue;
        }
        let escapes = entry.starts_with('/')
            || entry.starts_with('\\')
            || entry.contains(':')
            || entry.split(['/', '\\']).any(|component| component == "..")
            || Path::new(entry)
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir));
        if escapes {
            return Err(SttError::InvalidArgument(format!(
                "runtime archive entry `{entry}` escapes the runtime directory"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeArchiveFormat {
    Zip,
    Tar,
}

fn detect_runtime_archive_format<R: Read + Seek>(
    reader: &mut R,
) -> Result<RuntimeArchiveFormat, SttError> {
    let mut prefix = [0_u8; 512];
    let read = reader
        .read(&mut prefix)
        .map_err(|error| SttError::Io(format!("could not read runtime archive: {error}")))?;
    reader
        .rewind()
        .map_err(|error| SttError::Io(format!("could not seek runtime archive: {error}")))?;

    let zip_magic = matches!(
        prefix.get(..4),
        Some(b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08")
    );
    if zip_magic {
        return Ok(RuntimeArchiveFormat::Zip);
    }
    if read >= 262 && prefix.get(257..262) == Some(b"ustar") {
        return Ok(RuntimeArchiveFormat::Tar);
    }
    Err(SttError::InvalidArgument(
        "runtime archive format is not recognized".into(),
    ))
}

fn open_runtime_zip(path: &Path) -> Result<zip::ZipArchive<std::fs::File>, SttError> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| SttError::Io(format!("could not open runtime archive: {error}")))?;
    match detect_runtime_archive_format(&mut file)? {
        RuntimeArchiveFormat::Zip => zip::ZipArchive::new(file)
            .map_err(|error| SttError::Io(format!("could not read runtime archive: {error}"))),
        RuntimeArchiveFormat::Tar => Err(SttError::InvalidArgument(
            "runtime archive is tar, but the pinned runtime package must be zip".into(),
        )),
    }
}

fn zip_entry_is_symlink(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170_000 == 0o120_000)
}

fn screen_runtime_zip(archive: &mut zip::ZipArchive<std::fs::File>) -> Result<(), SttError> {
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| {
            SttError::Io(format!("could not read runtime archive entry: {error}"))
        })?;
        reject_unsafe_zip_entries(entry.name())?;
        if zip_entry_is_symlink(entry.unix_mode()) {
            return Err(SttError::InvalidArgument(format!(
                "runtime archive entry `{}` is a symbolic link",
                entry.name()
            )));
        }
    }
    Ok(())
}

fn metadata_is_symlink_like(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn reject_existing_symlink(path: &Path) -> Result<(), SttError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_symlink_like(&metadata) => {
            Err(SttError::InvalidArgument(format!(
                "runtime archive destination `{}` traverses a symbolic link",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SttError::Io(format!(
            "could not inspect runtime archive destination: {error}"
        ))),
    }
}

fn ensure_directory_component(path: &Path) -> Result<(), SttError> {
    reject_existing_symlink(path)?;
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(path).map_err(|error| {
                SttError::Io(format!(
                    "could not inspect runtime archive directory: {error}"
                ))
            })?;
            if metadata_is_symlink_like(&metadata) {
                return Err(SttError::InvalidArgument(format!(
                    "runtime archive destination `{}` traverses a symbolic link",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(SttError::Io(format!(
                    "runtime archive directory `{}` is not a directory",
                    path.display()
                )))
            }
        }
        Err(error) => Err(SttError::Io(format!(
            "could not create runtime archive directory: {error}"
        ))),
    }
}

fn ensure_directory_without_symlinks(runtime_dir: &Path, directory: &Path) -> Result<(), SttError> {
    reject_existing_symlink(runtime_dir)?;
    std::fs::create_dir_all(runtime_dir)
        .map_err(|error| SttError::Io(format!("could not create runtime directory: {error}")))?;
    reject_existing_symlink(runtime_dir)?;

    let relative = directory.strip_prefix(runtime_dir).map_err(|_| {
        SttError::InvalidArgument(
            "runtime archive destination escapes the runtime directory".into(),
        )
    })?;
    let mut current = runtime_dir.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::Normal(component) => current.push(component),
            _ => {
                return Err(SttError::InvalidArgument(
                    "runtime archive destination escapes the runtime directory".into(),
                ));
            }
        }
        ensure_directory_component(&current)?;
    }
    Ok(())
}

fn extract_screened_runtime_zip(
    archive: &mut zip::ZipArchive<std::fs::File>,
    runtime_dir: &Path,
) -> Result<(), SttError> {
    ensure_directory_without_symlinks(runtime_dir, runtime_dir)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            SttError::Io(format!("could not read runtime archive entry: {error}"))
        })?;
        let destination = runtime_dir.join(entry.name());
        if entry.is_dir() {
            ensure_directory_without_symlinks(runtime_dir, &destination)?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            ensure_directory_without_symlinks(runtime_dir, parent)?;
        }
        reject_existing_symlink(&destination)?;
        let mut output = std::fs::File::create(&destination).map_err(|error| {
            SttError::Io(format!("could not create runtime archive file: {error}"))
        })?;
        std::io::copy(&mut entry, &mut output).map_err(|error| {
            SttError::Io(format!("could not extract runtime archive file: {error}"))
        })?;
        output.flush().map_err(|error| {
            SttError::Io(format!("could not flush runtime archive file: {error}"))
        })?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(mode & 0o777))
                .map_err(|error| {
                SttError::Io(format!(
                    "could not set runtime archive permissions: {error}"
                ))
            })?;
        }
    }
    Ok(())
}

fn extract_runtime_zip_blocking(zip_path: &Path, runtime_dir: &Path) -> Result<PathBuf, SttError> {
    let mut archive = open_runtime_zip(zip_path)?;
    screen_runtime_zip(&mut archive)?;
    extract_screened_runtime_zip(&mut archive, runtime_dir)?;
    find_runtime_in_dir(runtime_dir).ok_or_else(|| SttError::RuntimeMissing {
        hint: "extracted runtime archive contains no whisper executable".into(),
    })
}

/// Extracts a VERIFIED runtime zip into `runtime_dir`.
///
/// Entry names are listed and screened by [`reject_unsafe_zip_entries`]
/// before any byte is extracted; extraction success requires a discoverable
/// runtime executable afterwards.
pub async fn extract_runtime_zip(zip_path: &Path, runtime_dir: &Path) -> Result<PathBuf, SttError> {
    let zip_path = zip_path.to_path_buf();
    let runtime_dir = runtime_dir.to_path_buf();
    tokio::task::spawn_blocking(move || extract_runtime_zip_blocking(&zip_path, &runtime_dir))
        .await
        .map_err(|error| SttError::Io(format!("runtime archive worker failed: {error}")))?
}

/// Downloads (verify-before-rename) and extracts the pinned Windows runtime
/// package into `<whisper_dir>/runtime/`.
pub async fn install_runtime_from_zip(
    client: &reqwest::Client,
    whisper_dir: &Path,
    spec: DownloadSpec<'_>,
    progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf, SttError> {
    let zip_path = install(client, whisper_dir, spec, progress).await?;
    extract_runtime_zip(&zip_path, &runtime_directory(whisper_dir)).await
}

/// The per-platform install driver.
///
/// macOS drives Homebrew (missing brew → honest `Unavailable` hint);
/// Windows drives the pinned zip; Linux reports the PATH-only hint. A
/// successful driver run still requires re-discovery by the caller — the
/// driver never guesses the installed path.
pub async fn install_runtime(
    client: &reqwest::Client,
    whisper_dir: &Path,
    progress: impl FnMut(DownloadProgress),
) -> Result<RuntimeInstallOutcome, SttError> {
    #[cfg(target_os = "macos")]
    {
        let _ = (client, whisper_dir, progress);
        let Some(brew) = homebrew_path_with(std::env::var_os("PATH").as_deref()) else {
            return Ok(RuntimeInstallOutcome::Unavailable {
                hint: HOMEBREW_MISSING_HINT.into(),
            });
        };
        install_runtime_with_homebrew(&brew).await?;
        Ok(RuntimeInstallOutcome::Installed)
    }
    #[cfg(windows)]
    {
        install_runtime_from_zip(
            client,
            whisper_dir,
            DownloadSpec {
                url: WINDOWS_RUNTIME_ZIP_URL,
                sha256: WINDOWS_RUNTIME_ZIP_SHA256,
                file_name: WINDOWS_RUNTIME_ZIP_FILE,
            },
            progress,
        )
        .await?;
        Ok(RuntimeInstallOutcome::Installed)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = (client, whisper_dir, progress);
        Ok(RuntimeInstallOutcome::Unavailable {
            hint: RUNTIME_INSTALL_HINT.into(),
        })
    }
}
