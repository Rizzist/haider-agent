//! Filesystem content-addressed storage. Owns the addressing scheme:
//!
//! - An artifact is addressed `blake3:<64 lowercase hex>` (the BLAKE3 hash of
//!   its bytes) and stored at `<profile>/cas/<first 2 hex>/<full hex>`.
//!   Identical bytes therefore deduplicate to one object; objects are
//!   immutable and write-once.
//! - Writes are atomic and durable: bytes go to a temp file in the target
//!   shard, are fsynced, hard-linked into place without replacing an existing
//!   object, and the shard directory is fsynced. A reader never observes a
//!   partially written object.
//! - Corruption is detected, not prevented: `get` and `verify` re-hash the
//!   bytes and compare against the address on every call.

use crate::{Cas, StoreResult, store_error};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::ArtifactRef;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide counter making temp-file names unique across threads.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Filesystem content-addressed storage rooted at `<profile>/cas`.
#[derive(Debug, Clone)]
pub struct FileCas {
    root: PathBuf,
}

impl FileCas {
    /// Opens a CAS in a profile root, creating its directory if needed.
    pub fn open(profile_root: impl AsRef<Path>) -> StoreResult<Self> {
        let profile_root = profile_root.as_ref();
        let root = profile_root.join("cas");
        let created = !root.exists();
        fs::create_dir_all(&root).map_err(|error| io_error("create CAS root", &root, error))?;
        if created {
            // Persist the new `cas/` directory entry itself.
            sync_directory(profile_root)?;
        }
        Ok(Self { root })
    }

    /// Resolves a validated artifact reference to its on-disk path.
    pub fn path_for(&self, artifact: &ArtifactRef) -> StoreResult<PathBuf> {
        let hex = parse_artifact_ref(artifact)?;
        Ok(self.root.join(&hex[..2]).join(hex))
    }

    /// Returns whether the object at `path` exists with bytes matching
    /// `artifact`; missing is `Ok(false)`, unreadable is an error.
    fn verify_existing(&self, artifact: &ArtifactRef, path: &Path) -> StoreResult<bool> {
        match File::open(path) {
            Ok(file) => Ok(artifact_for_reader(file, path)? == *artifact),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("read CAS object", path, error)),
        }
    }
}

impl Cas for FileCas {
    fn put(&self, bytes: &[u8]) -> StoreResult<ArtifactRef> {
        let artifact = artifact_for(bytes);
        let path = self.path_for(&artifact)?;
        if self.verify_existing(&artifact, &path)? {
            return Ok(artifact);
        }

        let parent = path.parent().ok_or_else(|| {
            store_error(
                ErrorCode::Internal,
                format!("CAS object has no parent directory: {}", path.display()),
                false,
            )
        })?;
        let shard_created = !parent.exists();
        fs::create_dir_all(parent).map_err(|error| io_error("create CAS shard", parent, error))?;
        if shard_created {
            // Persist the new shard directory entry itself.
            sync_directory(&self.root)?;
        }
        let (mut temporary_path, mut temporary) = create_temporary(parent)?;

        let write_result = temporary
            .write_all(bytes)
            .and_then(|()| temporary.sync_all());
        if let Err(error) = write_result {
            drop(temporary);
            let write_error =
                io_error("persist temporary CAS object", temporary_path.path(), error);
            cleanup_temporary(&mut temporary_path, parent)?;
            return Err(write_error);
        }
        drop(temporary);

        match fs::hard_link(temporary_path.path(), &path) {
            Ok(()) => {
                cleanup_temporary(&mut temporary_path, parent)?;
                Ok(artifact)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                // A racing putter won publication without being overwritten.
                // Verify its bytes before treating the content as deduplicated.
                let winner_is_valid = self.verify_existing(&artifact, &path);
                cleanup_temporary(&mut temporary_path, parent)?;
                if winner_is_valid? {
                    Ok(artifact)
                } else {
                    Err(corrupt_object(&path))
                }
            }
            Err(error) => {
                let publish_error = io_error("publish CAS object", &path, error);
                cleanup_temporary(&mut temporary_path, parent)?;
                Err(publish_error)
            }
        }
    }

    fn put_file(&self, source_path: &Path) -> StoreResult<ArtifactRef> {
        let mut source = File::open(source_path)
            .map_err(|error| io_error("open CAS source", source_path, error))?;
        let artifact = artifact_for_reader(&mut source, source_path)?;
        source
            .rewind()
            .map_err(|error| io_error("rewind CAS source", source_path, error))?;
        let path = self.path_for(&artifact)?;
        if self.verify_existing(&artifact, &path)? {
            return Ok(artifact);
        }

        let parent = path.parent().ok_or_else(|| {
            store_error(
                ErrorCode::Internal,
                format!("CAS object has no parent directory: {}", path.display()),
                false,
            )
        })?;
        let shard_created = !parent.exists();
        fs::create_dir_all(parent).map_err(|error| io_error("create CAS shard", parent, error))?;
        if shard_created {
            sync_directory(&self.root)?;
        }
        let (mut temporary_path, mut temporary) = create_temporary(parent)?;
        let write_result =
            std::io::copy(&mut source, &mut temporary).and_then(|_| temporary.sync_all());
        if let Err(error) = write_result {
            drop(temporary);
            let write_error =
                io_error("persist temporary CAS object", temporary_path.path(), error);
            cleanup_temporary(&mut temporary_path, parent)?;
            return Err(write_error);
        }
        drop(temporary);

        match fs::hard_link(temporary_path.path(), &path) {
            Ok(()) => {
                cleanup_temporary(&mut temporary_path, parent)?;
                Ok(artifact)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let winner_is_valid = self.verify_existing(&artifact, &path);
                cleanup_temporary(&mut temporary_path, parent)?;
                if winner_is_valid? {
                    Ok(artifact)
                } else {
                    Err(corrupt_object(&path))
                }
            }
            Err(error) => {
                let publish_error = io_error("publish CAS object", &path, error);
                cleanup_temporary(&mut temporary_path, parent)?;
                Err(publish_error)
            }
        }
    }

    fn get(&self, artifact: &ArtifactRef) -> StoreResult<Vec<u8>> {
        let path = self.path_for(artifact)?;
        let bytes = fs::read(&path).map_err(|error| {
            let code = if error.kind() == ErrorKind::NotFound {
                ErrorCode::InvalidArgument
            } else {
                ErrorCode::Internal
            };
            store_error(
                code,
                format!("cannot read CAS object {}: {error}", path.display()),
                false,
            )
        })?;
        if artifact_for(&bytes) != *artifact {
            return Err(corrupt_object(&path));
        }
        Ok(bytes)
    }

    fn verify(&self, artifact: &ArtifactRef) -> bool {
        self.path_for(artifact)
            .ok()
            .and_then(|path| self.verify_existing(artifact, &path).ok())
            .unwrap_or(false)
    }
}

/// Computes the canonical address for a byte string.
fn artifact_for(bytes: &[u8]) -> ArtifactRef {
    ArtifactRef::new(format!("blake3:{}", blake3::hash(bytes).to_hex()))
}

fn artifact_for_reader(mut reader: impl Read, path: &Path) -> StoreResult<ArtifactRef> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| io_error("hash CAS object", path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(ArtifactRef::new(format!(
        "blake3:{}",
        hasher.finalize().to_hex()
    )))
}

/// Extracts the hex digest from a `blake3:<hex>` reference, requiring exactly
/// 64 lowercase hex characters. Because the digest becomes a file name, this
/// validation is also the guard against path traversal via a crafted ref.
fn parse_artifact_ref(artifact: &ArtifactRef) -> StoreResult<&str> {
    let Some(hex) = artifact.as_str().strip_prefix("blake3:") else {
        return Err(invalid_artifact_ref(artifact));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_artifact_ref(artifact));
    }
    Ok(hex)
}

fn invalid_artifact_ref(artifact: &ArtifactRef) -> HaiderError {
    store_error(
        ErrorCode::InvalidArgument,
        format!("invalid BLAKE3 artifact reference: {artifact}"),
        false,
    )
}

/// Creates an exclusively owned temp file in `parent` for staging one object.
/// The pid + counter name is normally unique on the first try; the retries
/// only skip leftovers from a crashed process whose pid was recycled.
fn create_temporary(parent: &Path) -> StoreResult<(TemporaryPath, File)> {
    for _ in 0..32 {
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".tmp-{}-{counter}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((TemporaryPath::new(path), file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error("create temporary CAS object", &path, error)),
        }
    }
    Err(store_error(
        ErrorCode::Internal,
        format!(
            "cannot allocate a unique temporary CAS object in {}",
            parent.display()
        ),
        true,
    ))
}

/// Owns an unpublished temp path and removes it even when publication exits
/// early. Successful cleanup is explicit so its failure can be reported.
struct TemporaryPath {
    path: Option<PathBuf>,
}

impl TemporaryPath {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .unwrap_or_else(|| unreachable!("temporary path already removed"))
    }

    fn remove(&mut self) -> StoreResult<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        match fs::remove_file(path) {
            Ok(()) => {
                self.path = None;
                Ok(())
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                self.path = None;
                Ok(())
            }
            Err(error) => Err(io_error("remove temporary CAS object", path, error)),
        }
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

/// Removes a staged object and persists all directory mutations, including a
/// successful final hard link. The directory sync is attempted even if temp
/// removal fails, so a published object is still made durable.
fn cleanup_temporary(temporary: &mut TemporaryPath, parent: &Path) -> StoreResult<()> {
    let remove_result = temporary.remove();
    let sync_result = sync_directory(parent);
    remove_result?;
    sync_result
}

fn corrupt_object(path: &Path) -> HaiderError {
    store_error(
        ErrorCode::StoreCorrupt,
        format!("CAS object does not match its address: {}", path.display()),
        false,
    )
}

/// Fsyncs a directory so its entries (hard links, new subdirectories) survive a
/// crash.
fn sync_directory(path: &Path) -> StoreResult<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_error("sync directory", path, error))
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> HaiderError {
    store_error(
        ErrorCode::Internal,
        format!("{action} {}: {error}", path.display()),
        false,
    )
}
