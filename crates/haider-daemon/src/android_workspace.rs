//! Immutable Android workspace ceiling, independent of provider lockdown state.
use haider_protocol::error::{ErrorCode, HaiderError};
use std::path::Path;
#[cfg(unix)]
use std::path::{Component, PathBuf};
#[cfg(unix)]
use std::sync::OnceLock;

#[cfg(unix)]
struct WorkspaceCeiling {
    path: PathBuf,
    directory: haider_platform::WorkspaceDirectory,
}
#[cfg(unix)]
static CEILING: OnceLock<WorkspaceCeiling> = OnceLock::new();

#[cfg(all(test, unix, feature = "android-standalone"))]
#[allow(clippy::expect_used)]
pub(crate) fn test_root() -> &'static Path {
    static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
    let root = ROOT.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("a971")
            .tempdir_in("/tmp")
            .expect("Android workspace fixture")
    });
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    let path = PATH.get_or_init(|| root.path().canonicalize().expect("canonical fixture"));
    initialize(Some(path)).expect("initialize Android workspace fixture");
    path
}

pub(crate) fn initialize(path: Option<&Path>) -> Result<(), HaiderError> {
    if !crate::android_policy::enabled() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let path = path.ok_or_else(denied)?;
        if let Some(ceiling) = CEILING.get() {
            return if ceiling.path == path {
                Ok(())
            } else {
                Err(denied())
            };
        }
        if !path.is_absolute() || path.canonicalize().map_err(|_| denied())? != path {
            return Err(denied());
        }
        let directory = open_absolute(path)?;
        let ceiling = WorkspaceCeiling {
            path: path.to_path_buf(),
            directory,
        };
        if let Err(candidate) = CEILING.set(ceiling)
            && CEILING
                .get()
                .is_none_or(|existing| existing.path != candidate.path)
        {
            return Err(denied());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(denied())
    }
}

#[cfg(unix)]
fn open_absolute(path: &Path) -> Result<haider_platform::WorkspaceDirectory, HaiderError> {
    haider_platform::open_absolute_directory(path).map_err(|_| denied())
}

#[cfg(unix)]
fn descend(
    root: &haider_platform::WorkspaceDirectory,
    path: &Path,
) -> Result<haider_platform::WorkspaceDirectory, HaiderError> {
    use rustix::fs::{Mode, OFlags};
    let mut directory =
        haider_platform::duplicate_workspace_directory(root).map_err(|_| denied())?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(denied());
        };
        directory = rustix::fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| denied())?;
    }
    Ok(directory)
}

/// Returns a directory opened relative to the retained ceiling, never via an
/// untrusted session cwd ancestor. Filesystem tools retain this handle.
pub(crate) fn open(path: &Path) -> Result<haider_platform::WorkspaceDirectory, HaiderError> {
    if !crate::android_policy::enabled() {
        return haider_platform::open_workspace_directory(path).map_err(|_| denied());
    }
    #[cfg(unix)]
    {
        let ceiling = CEILING.get().ok_or_else(denied)?;
        let relative = path.strip_prefix(&ceiling.path).map_err(|_| denied())?;
        if relative
            .components()
            .any(|c| c.as_os_str() == ".haider-lockdown")
        {
            return Err(denied());
        }
        descend(&ceiling.directory, relative)
    }
    #[cfg(not(unix))]
    {
        Err(denied())
    }
}

pub(crate) fn validate(path: &Path) -> Result<(), HaiderError> {
    open(path).map(drop)
}

fn denied() -> HaiderError {
    HaiderError::new(
        ErrorCode::WorkspaceUnavailable,
        "workspace is outside the immutable Android workspace ceiling or is unavailable",
        false,
    )
}

pub(crate) fn private_sandbox_root() -> Result<std::path::PathBuf, HaiderError> {
    #[cfg(unix)]
    {
        Ok(CEILING
            .get()
            .ok_or_else(denied)?
            .path
            .join(".haider-lockdown"))
    }
    #[cfg(not(unix))]
    {
        Err(denied())
    }
}

#[cfg(all(unix, feature = "android-standalone"))]
pub(crate) fn open_private_sandbox() -> Result<haider_platform::WorkspaceDirectory, HaiderError> {
    use rustix::fs::{Mode, OFlags};
    let ceiling = CEILING.get().ok_or_else(denied)?;
    match rustix::fs::mkdirat(
        &ceiling.directory,
        ".haider-lockdown",
        Mode::from_raw_mode(0o700),
    ) {
        Ok(()) => rustix::fs::fsync(&ceiling.directory).map_err(|_| denied())?,
        Err(rustix::io::Errno::EXIST) => (),
        Err(_) => return Err(denied()),
    }
    let directory = rustix::fs::openat(
        &ceiling.directory,
        ".haider-lockdown",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| denied())?;
    let stat = rustix::fs::fstat(&directory).map_err(|_| denied())?;
    if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o777 != 0o700 {
        return Err(denied());
    }
    Ok(directory)
}

#[cfg(all(test, unix, feature = "android-standalone"))]
#[path = "android_workspace_tests.rs"]
mod tests;
