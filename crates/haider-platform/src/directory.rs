use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

#[cfg(unix)]
pub type WorkspaceDirectory = rustix::fd::OwnedFd;
#[cfg(windows)]
#[derive(Debug)]
pub struct WorkspaceDirectory {
    path: std::path::PathBuf,
    // Each handle omits FILE_SHARE_DELETE and opens the reparse point itself.
    // Retaining the full chain prevents an ancestor rename/junction swap from
    // retargeting a later path-based Windows API call.
    anchors: Vec<std::fs::File>,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowsFileIdentity {
    volume_serial: u32,
    file_index: u64,
}

#[cfg(windows)]
impl WorkspaceDirectory {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a DOS/UNC cwd that Windows PowerShell and other .NET Framework
    /// children can consume. A canonical `\\?\` path can make those children
    /// discard the requested working directory during initialization.
    ///
    /// Removing the verbatim prefix can change pathname semantics (for example,
    /// a trailing dot). Check the resulting directory identity while retaining
    /// the original root-to-cwd handles, and fail closed if it names another
    /// object. The caller must retain `self` until CreateProcess returns.
    pub fn process_path(&self) -> std::io::Result<PathBuf> {
        let path = process_directory_spelling(&self.path)?;
        let candidate = open_workspace_directory(&path)?;
        if workspace_directory_identity(&candidate)? != workspace_directory_identity(self)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "process cwd changes identity without the Windows verbatim prefix",
            ));
        }
        Ok(path)
    }
}

#[cfg(windows)]
fn process_directory_spelling(path: &Path) -> std::io::Result<PathBuf> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let mut result = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
            Prefix::VerbatimUNC(server, share) => {
                let mut unc = std::ffi::OsString::from(r"\\");
                unc.push(server);
                unc.push(r"\");
                unc.push(share);
                PathBuf::from(unc)
            }
            Prefix::Disk(_) | Prefix::UNC(_, _) => return Ok(path.to_path_buf()),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "process cwd has no DOS or UNC spelling",
                ));
            }
        },
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "process cwd must be an absolute DOS or UNC path",
            ));
        }
    };
    for component in components {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => result.push(name),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "process cwd must be canonical before changing its spelling",
                ));
            }
        }
    }
    Ok(result)
}

#[cfg(windows)]
impl std::ops::Deref for WorkspaceDirectory {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

#[cfg(windows)]
impl AsRef<Path> for WorkspaceDirectory {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

#[cfg(windows)]
impl AsRef<std::ffi::OsStr> for WorkspaceDirectory {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }
}

#[cfg(unix)]
pub type WorkspaceDirectoryError = rustix::io::Errno;
#[cfg(windows)]
pub type WorkspaceDirectoryError = std::io::Error;

/// Opens a canonical workspace root without following a final symlink.
#[cfg(unix)]
pub fn open_workspace_directory(
    path: &Path,
) -> Result<WorkspaceDirectory, WorkspaceDirectoryError> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

#[cfg(windows)]
pub fn open_workspace_directory(
    path: &Path,
) -> Result<WorkspaceDirectory, WorkspaceDirectoryError> {
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace directory must be absolute",
        ));
    }
    let mut components = path.ancestors().collect::<Vec<_>>();
    components.reverse();
    let anchors = components
        .into_iter()
        .map(open_anchored_directory)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(WorkspaceDirectory {
        path: path.to_path_buf(),
        anchors,
    })
}

#[cfg(unix)]
pub fn duplicate_workspace_directory(
    directory: &WorkspaceDirectory,
) -> Result<WorkspaceDirectory, WorkspaceDirectoryError> {
    rustix::io::dup(directory)
}

#[cfg(windows)]
pub fn duplicate_workspace_directory(
    directory: &WorkspaceDirectory,
) -> Result<WorkspaceDirectory, WorkspaceDirectoryError> {
    Ok(WorkspaceDirectory {
        path: directory.path.clone(),
        anchors: directory
            .anchors
            .iter()
            .map(std::fs::File::try_clone)
            .collect::<Result<_, _>>()?,
    })
}

/// Opens every relative directory component without following a reparse point
/// and retains the handle chain without delete-sharing.
#[cfg(windows)]
pub fn open_workspace_subdirectory(
    directory: WorkspaceDirectory,
    relative: &Path,
    create: bool,
) -> Result<WorkspaceDirectory, WorkspaceDirectoryError> {
    if relative.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace-relative directory path is absolute",
        ));
    }
    let mut current = directory.path;
    let mut anchors = directory.anchors;
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::Normal(name) => current.push(name),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "workspace-relative directory path escaped its root",
                ));
            }
        }
        let anchor = match open_anchored_directory(&current) {
            Ok(anchor) => anchor,
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                open_anchored_directory(&current)?
            }
            Err(error) => return Err(error),
        };
        anchors.push(anchor);
    }
    Ok(WorkspaceDirectory {
        path: current,
        anchors,
    })
}

/// Opens a regular file beneath an already anchored directory without
/// following any directory symlink/reparse point or the final file link.
///
/// The returned file handle fixes the object identity before the retained
/// directory chain is released, closing the authorization-to-open pathname
/// race for callers that read through the returned handle.
#[cfg(unix)]
pub fn open_workspace_file(
    mut directory: WorkspaceDirectory,
    relative: &Path,
) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};

    let mut components = checked_relative_components(relative)?;
    let leaf = components
        .pop()
        .ok_or_else(|| std::io::Error::other("workspace-relative file path has no leaf"))?;
    for component in components {
        directory = rustix::fs::openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
    }
    let file = std::fs::File::from(
        rustix::fs::openat(
            &directory,
            leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "workspace-relative path is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
pub fn open_workspace_file(
    directory: WorkspaceDirectory,
    relative: &Path,
) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    let mut components = checked_relative_components(relative)?;
    let leaf = components
        .pop()
        .ok_or_else(|| std::io::Error::other("workspace-relative file path has no leaf"))?;
    let parent = components.into_iter().collect::<PathBuf>();
    let directory = open_workspace_subdirectory(directory, &parent, false)?;
    let path = directory.path.join(leaf);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::other(format!(
            "{} is not a real regular file",
            path.display()
        )));
    }
    Ok(file)
}

fn checked_relative_components(relative: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    if relative.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace-relative file path is absolute",
        ));
    }
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => components.push(name.to_os_string()),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "workspace-relative file path escaped its root",
                ));
            }
        }
    }
    Ok(components)
}

#[cfg(windows)]
fn open_anchored_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    use std::os::windows::fs::MetadataExt as _;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::other(format!(
            "{} is not a real directory",
            path.display()
        )));
    }
    Ok(file)
}

#[cfg(windows)]
pub fn workspace_directory_identity(
    directory: &WorkspaceDirectory,
) -> std::io::Result<WindowsFileIdentity> {
    let anchor = directory
        .anchors
        .last()
        .ok_or_else(|| std::io::Error::other("workspace directory has no anchor handle"))?;
    windows_file_identity(anchor)
}

/// Reports whether an exact file identity occurs in the retained root-to-leaf
/// anchor chain. This is namespace-independent, so Windows case aliases and
/// 8.3 spellings cannot disguise a destination nested beneath a source.
#[cfg(windows)]
pub fn workspace_directory_contains_identity(
    directory: &WorkspaceDirectory,
    expected: WindowsFileIdentity,
) -> std::io::Result<bool> {
    for anchor in &directory.anchors {
        if windows_file_identity(anchor)? == expected {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn windows_file_identity(file: &std::fs::File) -> std::io::Result<WindowsFileIdentity> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns the live handle and `information` is writable for
    // exactly the structure size expected by GetFileInformationByHandle.
    let read = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut information) };
    if read == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(WindowsFileIdentity {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: u64::from(information.nFileIndexHigh) << 32
            | u64::from(information.nFileIndexLow),
    })
}

#[cfg(all(test, windows))]
mod windows_process_directory_tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn process_directory_spelling_preserves_drive_and_unc_components() {
        for (verbatim, ordinary) in [
            (r"\\?\C:\workspace\cwd", r"C:\workspace\cwd"),
            (
                r"\\?\UNC\server\share\workspace\cwd",
                r"\\server\share\workspace\cwd",
            ),
        ] {
            assert_eq!(
                process_directory_spelling(Path::new(verbatim)).expect("process cwd spelling"),
                Path::new(ordinary)
            );
        }
    }

    #[test]
    fn process_directory_rejects_normalized_alias_to_another_directory() {
        let workspace = tempfile::tempdir().expect("process cwd workspace");
        let canonical = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
        // NTFS permits a literal trailing dot through the verbatim namespace.
        // Removing that prefix would silently select the other directory.
        let ordinary = canonical.join("cwd");
        let verbatim = canonical.join("cwd.");
        std::fs::create_dir(&ordinary).expect("create ordinary cwd");
        std::fs::create_dir(&verbatim).expect("create distinct verbatim cwd");
        let ordinary = open_workspace_directory(&ordinary).expect("anchor ordinary cwd");
        let verbatim = open_workspace_directory(&verbatim).expect("anchor verbatim cwd");
        assert_ne!(
            workspace_directory_identity(&ordinary).expect("ordinary identity"),
            workspace_directory_identity(&verbatim).expect("verbatim identity"),
            "fixture directories must have distinct identities"
        );
        let error = verbatim
            .process_path()
            .expect_err("process cwd must not change identity");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("changes identity"));
    }
}
