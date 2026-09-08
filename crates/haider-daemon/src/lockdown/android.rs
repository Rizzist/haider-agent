//! Private provider data is anchored inside the immutable Android workspace.
use super::*;
use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, FileType, Mode, OFlags};

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn open_directory(parent: &OwnedFd, path: &Path, create: bool) -> Result<OwnedFd, LockdownError> {
    let mut directory = haider_platform::duplicate_workspace_directory(parent)
        .map_err(|error| io_error("duplicate sandbox", path, error.into()))?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(LockdownError::InvalidRelativePath { path: path.into() });
        };
        if create {
            match rustix::fs::mkdirat(&directory, name, Mode::from_raw_mode(0o700)) {
                Ok(()) => rustix::fs::fsync(&directory)
                    .map_err(|error| io_error("sync sandbox directory", path, error.into()))?,
                Err(rustix::io::Errno::EXIST) => (),
                Err(error) => return Err(io_error("create sandbox directory", path, error.into())),
            }
        }
        directory = rustix::fs::openat(&directory, name, DIRECTORY, Mode::empty())
            .map_err(|error| io_error("open sandbox directory", path, error.into()))?;
    }
    Ok(directory)
}

fn regular(metadata: &rustix::fs::Stat, path: &Path) -> Result<(), LockdownError> {
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile || metadata.st_nlink != 1
    {
        return Err(LockdownError::InvalidRelativePath { path: path.into() });
    }
    Ok(())
}

pub(super) fn size(directory: &OwnedFd) -> Result<u64, LockdownError> {
    let mut total = 0_u64;
    let mut pending = vec![open_directory(directory, Path::new(""), false)?];
    while let Some(directory) = pending.pop() {
        let mut entries = rustix::fs::Dir::read_from(&directory)
            .map_err(|error| io_error("list sandbox", Path::new("."), error.into()))?;
        for entry in &mut entries {
            let entry =
                entry.map_err(|error| io_error("list sandbox", Path::new("."), error.into()))?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let metadata = rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| io_error("inspect sandbox", Path::new("."), error.into()))?;
            if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
                pending.push(
                    rustix::fs::openat(&directory, name, DIRECTORY, Mode::empty())
                        .map_err(|error| io_error("open sandbox", Path::new("."), error.into()))?,
                );
            } else {
                regular(&metadata, Path::new("."))?;
                total = total.saturating_add(metadata.st_size.max(0) as u64);
            }
        }
    }
    Ok(total)
}

impl LockdownManager {
    pub(super) fn android_write(
        &self,
        provider: &str,
        relative: &Path,
        contents: &[u8],
        after_apply: impl FnOnce() -> Result<(), LockdownError>,
    ) -> Result<LockdownStatus, LockdownError> {
        let provider_root = self.provider_root(provider)?;
        let target = sandbox_path(&provider_root, relative)?;
        let mut applied = false;
        let result = self.with_locked_ledger(|ledger| {
            ledger.used = size(&self.sandbox_directory)?;
            if ledger.used.saturating_add(contents.len() as u64) > ledger.limit {
                return Err(LockdownError::LockdownQuotaExceeded {
                    used: ledger.used,
                    limit: ledger.limit,
                });
            }
            let relative = PathBuf::from(provider_slug(provider)?).join(relative);
            let parent = open_directory(
                &self.sandbox_directory,
                relative
                    .parent()
                    .ok_or_else(|| LockdownError::InvalidRelativePath {
                        path: relative.clone(),
                    })?,
                true,
            )?;
            let leaf = relative
                .file_name()
                .ok_or_else(|| LockdownError::InvalidRelativePath {
                    path: relative.clone(),
                })?;
            match rustix::fs::statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(metadata) => regular(&metadata, &target)?,
                Err(rustix::io::Errno::NOENT) => (),
                Err(error) => {
                    return Err(io_error("inspect sandbox target", &target, error.into()));
                }
            }
            let temporary = data_temporary_name(&target)?;
            let mut file = File::from(
                rustix::fs::openat(
                    &parent,
                    temporary.as_str(),
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                )
                .map_err(|error| io_error("create sandbox temporary", &target, error.into()))?,
            );
            let write = (|| {
                file.write_all(contents)
                    .map_err(|error| io_error("write sandbox temporary", &target, error))?;
                file.sync_all()
                    .map_err(|error| io_error("sync sandbox temporary", &target, error))?;
                rustix::fs::renameat(&parent, temporary.as_str(), &parent, leaf)
                    .map_err(|error| io_error("replace sandbox target", &target, error.into()))?;
                applied = true;
                rustix::fs::fsync(&parent)
                    .map_err(|error| io_error("sync sandbox directory", &target, error.into()))?;
                after_apply()
            })();
            if !applied {
                let _ = rustix::fs::unlinkat(&parent, temporary.as_str(), AtFlags::empty());
            }
            write?;
            ledger.used = size(&self.sandbox_directory)?;
            Ok(LockdownStatus {
                provider: Some(provider.into()),
                sandbox: Some(provider_root),
                tools_allowed: allowed_tool_names(),
                quota_used: ledger.used,
                quota_limit: ledger.limit,
            })
        });
        result.map_err(|error| {
            if applied {
                LockdownError::AppliedWrite(Box::new(error))
            } else {
                error
            }
        })
    }

    pub(super) fn android_read(
        &self,
        provider: &str,
        relative: &Path,
    ) -> Result<Vec<u8>, LockdownError> {
        if !relative.as_os_str().is_empty() {
            sandbox_path(&self.provider_root(provider)?, relative)?;
        }
        let path = PathBuf::from(provider_slug(provider)?).join(relative);
        let parent = open_directory(
            &self.sandbox_directory,
            path.parent()
                .ok_or_else(|| LockdownError::InvalidRelativePath { path: path.clone() })?,
            false,
        )?;
        let leaf = path
            .file_name()
            .ok_or_else(|| LockdownError::InvalidRelativePath { path: path.clone() })?;
        let file = rustix::fs::openat(
            &parent,
            leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| io_error("read sandbox", &path, error.into()))?;
        let metadata = rustix::fs::fstat(&file)
            .map_err(|error| io_error("inspect sandbox", &path, error.into()))?;
        if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
            let mut entries = rustix::fs::Dir::read_from(&file)
                .map_err(|error| io_error("list sandbox", &path, error.into()))?;
            let mut names = Vec::new();
            for entry in (&mut entries).take(MAX_DIRECTORY_ENTRIES) {
                let entry = entry.map_err(|error| io_error("list sandbox", &path, error.into()))?;
                if matches!(entry.file_name().to_bytes(), b"." | b"..") {
                    continue;
                }
                let metadata =
                    rustix::fs::statat(&file, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                        .map_err(|error| io_error("inspect sandbox entry", &path, error.into()))?;
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
                    name.push('/');
                } else {
                    regular(&metadata, &path)?;
                }
                names.push(name);
            }
            names.sort();
            return Ok(names.join("\n").into_bytes());
        }
        regular(&metadata, &path)?;
        let mut bytes = Vec::new();
        File::from(file)
            .take(DEFAULT_QUOTA_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| io_error("read sandbox", &path, error))?;
        if bytes.len() as u64 > DEFAULT_QUOTA_BYTES {
            return Err(LockdownError::InvalidRelativePath { path });
        }
        Ok(bytes)
    }
}
