use std::path::Path;

#[cfg(unix)]
pub type WorkspaceDirectory = rustix::fd::OwnedFd;
#[cfg(windows)]
pub type WorkspaceDirectory = std::path::PathBuf;

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
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::other(
            "workspace root is not a real directory",
        ));
    }
    Ok(path.to_path_buf())
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
    Ok(directory.clone())
}
