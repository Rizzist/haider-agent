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
use haider_protocol::tool::{
    ImageBlockRef, TOOL_RESULT_IMAGE_MAX_BYTES, TOOL_RESULT_IMAGE_MAX_DECODE_ALLOC,
    TOOL_RESULT_IMAGE_MAX_DIMENSION, TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES,
    TOOL_RESULT_IMAGE_MAX_SOURCE_PIXELS,
};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
#[path = "cas_tests.rs"]
mod cas_tests;

/// Process-wide counter making temp-file names unique across threads.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CasSyncTarget {
    File,
    Directory,
}

#[cfg(test)]
type CasSyncTestHook = Box<dyn FnMut(&Path, haider_platform::SyncPolicy, CasSyncTarget)>;

#[cfg(test)]
std::thread_local! {
    static CAS_SYNC_TEST_HOOK: std::cell::RefCell<Option<CasSyncTestHook>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn intercept_sync_for_test(
    path: &Path,
    policy: haider_platform::SyncPolicy,
    target: CasSyncTarget,
) -> bool {
    CAS_SYNC_TEST_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(hook) = slot.as_mut() else {
            return false;
        };
        hook(path, policy, target);
        true
    })
}

#[cfg(test)]
pub(crate) fn with_cas_sync_test_hook<T>(
    hook: impl FnMut(&Path, haider_platform::SyncPolicy, CasSyncTarget) + 'static,
    action: impl FnOnce() -> T,
) -> T {
    let previous = CAS_SYNC_TEST_HOOK.with(|slot| slot.replace(Some(Box::new(hook))));
    let result = action();
    CAS_SYNC_TEST_HOOK.with(|slot| {
        slot.replace(previous);
    });
    result
}

/// Input bytes are bounded before format probing or decoding. The larger
/// source allowance lets ordinary lossless screenshots be recompressed and
/// downscaled into the existing 5 MiB artifact ceiling.
/// Filesystem content-addressed storage rooted at `<profile>/cas`.
#[derive(Debug, Clone)]
pub struct FileCas {
    root: PathBuf,
}

impl FileCas {
    /// Opens a CAS in a profile root, creating its directory if needed.
    pub fn open(profile_root: impl AsRef<Path>) -> StoreResult<Self> {
        Self::open_namespace(profile_root.as_ref(), "cas")
    }

    /// Opens one store-owned CAS namespace. Namespace names are compile-time
    /// constants supplied by this crate, never user-controlled paths.
    pub(crate) fn open_namespace(profile_root: &Path, namespace: &str) -> StoreResult<Self> {
        let root = profile_root.join(namespace);
        let created = !root.exists();
        fs::create_dir_all(&root).map_err(|error| io_error("create CAS root", &root, error))?;
        if created {
            // Persist the new `cas/` directory entry itself.
            sync_directory(profile_root, haider_platform::SyncPolicy::Full)?;
        }
        Ok(Self { root })
    }

    /// Removes one verified namespace object during reference-index GC.
    /// Missing objects are already swept. The shard directory is fsynced so
    /// Windows and Unix both durably observe the unlink.
    pub(crate) fn remove(&self, artifact: &ArtifactRef) -> StoreResult<()> {
        let path = self.path_for(artifact)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                let parent = path.parent().ok_or_else(|| {
                    store_error(
                        ErrorCode::Internal,
                        format!("CAS object has no parent directory: {}", path.display()),
                        false,
                    )
                })?;
                // Garbage-collection unlink durability is unchanged.
                sync_directory(parent, haider_platform::SyncPolicy::Full)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove CAS object", &path, error)),
        }
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

    pub(crate) fn verify_artifact(&self, artifact: &ArtifactRef) -> StoreResult<bool> {
        let path = self.path_for(artifact)?;
        self.verify_existing(artifact, &path)
    }

    /// Copies and hashes one reader in the same pass. The temporary lives in
    /// the CAS root because its digest (and therefore its shard) is not known
    /// until every byte that will be published has been written.
    fn put_reader(&self, mut source: impl Read, source_path: &Path) -> StoreResult<ArtifactRef> {
        let (mut temporary_path, mut temporary) = create_temporary(&self.root)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 16 * 1024];
        let copy_result = (|| {
            loop {
                let read = source
                    .read(&mut buffer)
                    .map_err(|error| io_error("read CAS source", source_path, error))?;
                if read == 0 {
                    break;
                }
                temporary.write_all(&buffer[..read]).map_err(|error| {
                    io_error("persist temporary CAS object", temporary_path.path(), error)
                })?;
                // Update only after write_all succeeds: the address covers
                // exactly the complete bytes eligible for publication.
                hasher.update(&buffer[..read]);
            }
            // Streaming CAS writes retain their independent full-durability boundary.
            sync_file(
                &temporary,
                temporary_path.path(),
                haider_platform::SyncPolicy::Full,
            )?;
            Ok(ArtifactRef::new(format!(
                "blake3:{}",
                hasher.finalize().to_hex()
            )))
        })();
        let artifact = match copy_result {
            Ok(artifact) => artifact,
            Err(error) => {
                drop(temporary);
                cleanup_temporary(
                    &mut temporary_path,
                    &self.root,
                    haider_platform::SyncPolicy::Full,
                )?;
                return Err(error);
            }
        };
        drop(temporary);

        let path = self.path_for(&artifact)?;
        if self.verify_existing(&artifact, &path)? {
            cleanup_temporary(
                &mut temporary_path,
                &self.root,
                haider_platform::SyncPolicy::Full,
            )?;
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
            // Independent streaming puts durably publish a newly created shard.
            sync_directory(&self.root, haider_platform::SyncPolicy::Full)?;
        }

        match fs::hard_link(temporary_path.path(), &path) {
            Ok(()) => {
                // Both directory mutations are durability boundaries: unlink
                // the root staging name and persist the shard publication.
                cleanup_temporary(
                    &mut temporary_path,
                    &self.root,
                    haider_platform::SyncPolicy::Full,
                )?;
                // Independent streaming puts durably publish the object link.
                sync_directory(parent, haider_platform::SyncPolicy::Full)?;
                Ok(artifact)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let winner_is_valid = self.verify_existing(&artifact, &path);
                cleanup_temporary(
                    &mut temporary_path,
                    &self.root,
                    haider_platform::SyncPolicy::Full,
                )?;
                if winner_is_valid? {
                    Ok(artifact)
                } else {
                    Err(corrupt_object(&path))
                }
            }
            Err(error) => {
                let publish_error = io_error("publish CAS object", &path, error);
                cleanup_temporary(
                    &mut temporary_path,
                    &self.root,
                    haider_platform::SyncPolicy::Full,
                )?;
                Err(publish_error)
            }
        }
    }

    /// Writes one member of a larger durability group. The caller must invoke
    /// [`Self::finish_batched_puts`] before committing any durable reference.
    pub(crate) fn put_batched(&self, bytes: &[u8]) -> StoreResult<ArtifactRef> {
        self.put_bytes(bytes, haider_platform::SyncPolicy::Plain, true)
    }

    /// Closes a durability group after every member received its plain file
    /// and directory sync, and before references enter SQLite or the journal.
    pub(crate) fn finish_batched_puts(&self) -> StoreResult<()> {
        // One full root flush closes the group and includes deferred shard creation.
        sync_directory(&self.root, haider_platform::SyncPolicy::Full)
    }

    fn put_bytes(
        &self,
        bytes: &[u8],
        policy: haider_platform::SyncPolicy,
        defer_shard_sync: bool,
    ) -> StoreResult<ArtifactRef> {
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
        if shard_created && !defer_shard_sync {
            // Independent puts retain the existing shard-publication boundary.
            sync_directory(&self.root, policy)?;
        }
        let (mut temporary_path, mut temporary) = create_temporary(parent)?;

        let write_result = temporary
            .write_all(bytes)
            .map_err(|error| io_error("persist temporary CAS object", temporary_path.path(), error))
            .and_then(|()| {
                // Each object is flushed before its directory entry can become durable.
                sync_file(&temporary, temporary_path.path(), policy)
            });
        if let Err(error) = write_result {
            drop(temporary);
            cleanup_temporary(&mut temporary_path, parent, policy)?;
            return Err(error);
        }
        drop(temporary);

        match fs::hard_link(temporary_path.path(), &path) {
            Ok(()) => {
                cleanup_temporary(&mut temporary_path, parent, policy)?;
                Ok(artifact)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                // A racing putter won publication without being overwritten.
                // Verify its bytes before treating the content as deduplicated.
                let winner_is_valid = self.verify_existing(&artifact, &path);
                cleanup_temporary(&mut temporary_path, parent, policy)?;
                if winner_is_valid? {
                    Ok(artifact)
                } else {
                    Err(corrupt_object(&path))
                }
            }
            Err(error) => {
                let publish_error = io_error("publish CAS object", &path, error);
                cleanup_temporary(&mut temporary_path, parent, policy)?;
                Err(publish_error)
            }
        }
    }

    fn bound_image(&self, bytes: Vec<u8>, media_type: &str) -> StoreResult<(Vec<u8>, u32, u32)> {
        if bytes.len() > TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES {
            return Err(invalid_image(format!(
                "tool image source is {} bytes; the source limit is {TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES}",
                bytes.len()
            )));
        }
        let format = image_format(media_type)?;
        let (source_width, source_height, decoded) = match format {
            ToolImageFormat::Png => {
                let detected = image::guess_format(&bytes).map_err(|error| {
                    invalid_image(format!("tool image format is invalid: {error}"))
                })?;
                if detected != ImageFormat::Png {
                    return Err(invalid_image(format!(
                        "tool image declares `{media_type}` but its encoded format is not PNG"
                    )));
                }
                let dimensions = ImageReader::with_format(Cursor::new(&bytes), ImageFormat::Png)
                    .into_dimensions()
                    .map_err(|error| {
                        invalid_image(format!("tool image dimensions are invalid: {error}"))
                    })?;
                validate_source_dimensions(dimensions.0, dimensions.1)?;
                let decoded = decode_png(&bytes)?;
                (dimensions.0, dimensions.1, Some(decoded))
            }
            ToolImageFormat::Jpeg => {
                let (width, height) = jpeg_dimensions(&bytes)?;
                (width, height, None)
            }
        };
        validate_source_dimensions(source_width, source_height)?;
        if source_width <= TOOL_RESULT_IMAGE_MAX_DIMENSION
            && source_height <= TOOL_RESULT_IMAGE_MAX_DIMENSION
            && u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= TOOL_RESULT_IMAGE_MAX_BYTES
        {
            return Ok((bytes, source_width, source_height));
        }

        if format == ToolImageFormat::Jpeg {
            return Err(invalid_image(format!(
                "oversized JPEG tool image is {source_width}x{source_height} / {} bytes; encode it within {}x{} and {TOOL_RESULT_IMAGE_MAX_BYTES} bytes before storage",
                bytes.len(),
                TOOL_RESULT_IMAGE_MAX_DIMENSION,
                TOOL_RESULT_IMAGE_MAX_DIMENSION
            )));
        }
        let decoded = decoded.ok_or_else(|| {
            invalid_image("oversized JPEG tool images require a bounded JPEG decoder")
        })?;
        drop(bytes);
        let mut target_dimension = TOOL_RESULT_IMAGE_MAX_DIMENSION;
        loop {
            let bounded = resize_to_fit(&decoded, target_dimension);
            let width = bounded.width();
            let height = bounded.height();
            let encoded = encode_image(&bounded)?;
            if u64::try_from(encoded.len()).unwrap_or(u64::MAX) <= TOOL_RESULT_IMAGE_MAX_BYTES {
                return Ok((encoded, width, height));
            }
            if target_dimension <= 256 {
                return Err(invalid_image(format!(
                    "tool image cannot be encoded within {TOOL_RESULT_IMAGE_MAX_BYTES} bytes"
                )));
            }
            target_dimension = target_dimension.saturating_mul(3) / 4;
        }
    }
}

/// Verifies that a durable image ref describes the exact bounded CAS bytes.
/// This is also used at the tool-result journal boundary so callers cannot
/// bypass [`Cas::put_image`] with an arbitrary generic artifact ref.
pub fn validate_image_block(bytes: &[u8], image: &ImageBlockRef) -> StoreResult<()> {
    if artifact_for(bytes) != image.artifact {
        return Err(invalid_image(format!(
            "tool image {} does not address the supplied bytes",
            image.artifact
        )));
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != image.byte_len
        || image.byte_len == 0
        || image.byte_len > TOOL_RESULT_IMAGE_MAX_BYTES
    {
        return Err(invalid_image(format!(
            "tool image {} byte length disagrees with its bounded metadata",
            image.artifact
        )));
    }
    let (width, height) = match image_format(&image.media_type)? {
        ToolImageFormat::Png => {
            let dimensions = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png)
                .into_dimensions()
                .map_err(|error| {
                    invalid_image(format!("tool image dimensions are invalid: {error}"))
                })?;
            if dimensions != (image.width, image.height)
                || dimensions.0 > TOOL_RESULT_IMAGE_MAX_DIMENSION
                || dimensions.1 > TOOL_RESULT_IMAGE_MAX_DIMENSION
            {
                return Err(invalid_image(format!(
                    "tool image {} dimensions disagree with its bounded metadata",
                    image.artifact
                )));
            }
            let decoded = decode_png(bytes)?;
            (decoded.width(), decoded.height())
        }
        ToolImageFormat::Jpeg => jpeg_dimensions(bytes)?,
    };
    if width == 0
        || height == 0
        || width > TOOL_RESULT_IMAGE_MAX_DIMENSION
        || height > TOOL_RESULT_IMAGE_MAX_DIMENSION
        || width != image.width
        || height != image.height
    {
        return Err(invalid_image(format!(
            "tool image {} dimensions disagree with its bounded metadata",
            image.artifact
        )));
    }
    Ok(())
}

impl Cas for FileCas {
    fn put(&self, bytes: &[u8]) -> StoreResult<ArtifactRef> {
        self.put_bytes(bytes, haider_platform::SyncPolicy::Full, false)
    }

    fn put_batch(&self, blobs: &[Vec<u8>]) -> StoreResult<Vec<ArtifactRef>> {
        let artifacts = blobs
            .iter()
            .map(|bytes| self.put_batched(bytes))
            .collect::<StoreResult<Vec<_>>>()?;
        if !blobs.is_empty() {
            self.finish_batched_puts()?;
        }
        Ok(artifacts)
    }

    fn put_file(&self, source_path: &Path) -> StoreResult<ArtifactRef> {
        let source = File::open(source_path)
            .map_err(|error| io_error("open CAS source", source_path, error))?;
        self.put_reader(source, source_path)
    }

    fn put_image(&self, bytes: Vec<u8>, media_type: &str) -> StoreResult<ImageBlockRef> {
        let (bounded, width, height) = self.bound_image(bytes, media_type)?;
        let byte_len = u64::try_from(bounded.len()).map_err(|_| {
            invalid_image("bounded tool image length does not fit the protocol counter")
        })?;
        let artifact = Cas::put(self, &bounded)?;
        Ok(ImageBlockRef {
            artifact,
            media_type: media_type.to_owned(),
            width,
            height,
            byte_len,
        })
    }

    fn get(&self, artifact: &ArtifactRef) -> StoreResult<Vec<u8>> {
        let path = self.path_for(artifact)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot read CAS object {}: {error}", path.display()),
                    false,
                ));
            }
            Err(error) => return Err(io_error("read CAS object", &path, error)),
        };
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolImageFormat {
    Png,
    Jpeg,
}

fn image_format(media_type: &str) -> StoreResult<ToolImageFormat> {
    match media_type {
        "image/png" => Ok(ToolImageFormat::Png),
        "image/jpeg" => Ok(ToolImageFormat::Jpeg),
        _ => Err(invalid_image(format!(
            "unsupported tool image media type `{media_type}`; use image/png or image/jpeg"
        ))),
    }
}

fn validate_source_dimensions(width: u32, height: u32) -> StoreResult<()> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0 || height == 0 || pixels > TOOL_RESULT_IMAGE_MAX_SOURCE_PIXELS {
        return Err(invalid_image(format!(
            "tool image dimensions {width}x{height} exceed the safe decode limit"
        )));
    }
    Ok(())
}

fn decode_png(bytes: &[u8]) -> StoreResult<DynamicImage> {
    validate_png_container(bytes)?;
    let detected = image::guess_format(bytes)
        .map_err(|error| invalid_image(format!("tool image format is invalid: {error}")))?;
    if detected != ImageFormat::Png {
        return Err(invalid_image(
            "tool image declares `image/png` but its encoded format is not PNG",
        ));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_alloc = Some(TOOL_RESULT_IMAGE_MAX_DECODE_ALLOC);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|error| invalid_image(format!("tool image could not be decoded: {error}")))
}

fn validate_png_container(bytes: &[u8]) -> StoreResult<()> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIGNATURE) {
        return Err(invalid_image(
            "tool image declares `image/png` but has no PNG signature",
        ));
    }
    let mut cursor = SIGNATURE.len();
    let mut saw_header = false;
    let mut saw_data = false;
    while cursor < bytes.len() {
        let header = bytes
            .get(cursor..cursor.saturating_add(8))
            .ok_or_else(|| invalid_image("tool image PNG has a truncated chunk header"))?;
        let data_len = usize::try_from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]))
        .map_err(|_| invalid_image("tool image PNG chunk length is invalid"))?;
        let chunk_type = &header[4..8];
        let data_start = cursor.saturating_add(8);
        let data_end = data_start
            .checked_add(data_len)
            .ok_or_else(|| invalid_image("tool image PNG chunk length overflows"))?;
        let chunk_end = data_end
            .checked_add(4)
            .ok_or_else(|| invalid_image("tool image PNG chunk length overflows"))?;
        let data = bytes
            .get(data_start..data_end)
            .ok_or_else(|| invalid_image("tool image PNG has truncated chunk data"))?;
        let crc_bytes = bytes
            .get(data_end..chunk_end)
            .ok_or_else(|| invalid_image("tool image PNG has a truncated chunk CRC"))?;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(chunk_type);
        hasher.update(data);
        let expected_crc =
            u32::from_be_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
        if hasher.finalize() != expected_crc {
            return Err(invalid_image("tool image PNG has an invalid chunk CRC"));
        }

        match chunk_type {
            b"IHDR" if !saw_header && cursor == SIGNATURE.len() && data_len == 13 => {
                saw_header = true;
            }
            b"IHDR" => return Err(invalid_image("tool image PNG has an invalid IHDR chunk")),
            b"IDAT" if saw_header => saw_data = true,
            b"IEND" if saw_header && saw_data && data_len == 0 => {
                if chunk_end != bytes.len() {
                    return Err(invalid_image(
                        "tool image PNG has bytes after its IEND chunk",
                    ));
                }
                return Ok(());
            }
            b"IEND" => return Err(invalid_image("tool image PNG has an invalid IEND chunk")),
            _ if !saw_header => {
                return Err(invalid_image("tool image PNG does not begin with IHDR"));
            }
            _ => {}
        }
        cursor = chunk_end;
    }
    Err(invalid_image(
        "tool image PNG is missing a complete terminal IEND chunk",
    ))
}

fn jpeg_dimensions(bytes: &[u8]) -> StoreResult<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(invalid_image(
            "tool image declares `image/jpeg` but has no JPEG signature",
        ));
    }
    let mut cursor = 2_usize;
    let mut dimensions = None;
    let mut frame_components = Vec::new();
    let mut frame_quantization_tables = Vec::new();
    let mut scanned_components = Vec::new();
    let mut frame_marker = None;
    let mut quantization_tables = [false; 4];
    let mut dc_huffman_tables = [false; 4];
    let mut ac_huffman_tables = [false; 4];
    let mut saw_scan = false;
    let mut in_scan = false;
    let mut current_scan_has_data = false;
    let mut restart_interval = 0_u16;
    let mut expected_restart = 0_u8;
    while cursor < bytes.len() {
        if in_scan {
            if bytes[cursor] != 0xff {
                current_scan_has_data = true;
                cursor = cursor.saturating_add(1);
                continue;
            }
            let Some(&next) = bytes.get(cursor.saturating_add(1)) else {
                break;
            };
            if next == 0x00 {
                current_scan_has_data = true;
                cursor = cursor.saturating_add(2);
                continue;
            }
            if next == 0xff {
                cursor = cursor.saturating_add(1);
                continue;
            }
            if (0xd0..=0xd7).contains(&next) {
                if restart_interval == 0
                    || !current_scan_has_data
                    || next != 0xd0 + expected_restart
                {
                    return Err(invalid_image(
                        "tool image JPEG has an invalid restart-marker sequence",
                    ));
                }
                expected_restart = (expected_restart + 1) % 8;
                current_scan_has_data = false;
                cursor = cursor.saturating_add(2);
                continue;
            }
            if !current_scan_has_data {
                return Err(invalid_image(
                    "tool image declares `image/jpeg` but has an empty scan",
                ));
            }
            in_scan = false;
        }
        if bytes.get(cursor) != Some(&0xff) {
            return Err(invalid_image(
                "tool image declares `image/jpeg` but contains bytes outside a scan",
            ));
        }
        while cursor < bytes.len() && bytes[cursor] == 0xff {
            cursor = cursor.saturating_add(1);
        }
        let Some(&marker) = bytes.get(cursor) else {
            break;
        };
        cursor = cursor.saturating_add(1);
        if marker == 0xd9 {
            let Some(dimensions) = dimensions else {
                break;
            };
            if cursor != bytes.len() || !saw_scan {
                break;
            }
            if frame_components
                .iter()
                .any(|component| !scanned_components.contains(component))
            {
                break;
            }
            return Ok(dimensions);
        }
        if marker == 0xd8 || marker == 0x00 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            return Err(invalid_image(
                "tool image declares `image/jpeg` but has an invalid marker order",
            ));
        }
        let Some(length_bytes) = bytes.get(cursor..cursor.saturating_add(2)) else {
            break;
        };
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 || cursor.saturating_add(length) > bytes.len() {
            break;
        }
        let segment = &bytes[cursor + 2..cursor + length];
        match marker {
            0xc0 => {
                if dimensions.is_some() || segment.len() < 6 || segment[0] != 8 {
                    break;
                }
                let component_count = usize::from(segment[5]);
                if component_count == 0
                    || component_count > 4
                    || segment.len() != 6 + component_count * 3
                {
                    break;
                }
                let components = &segment[6..];
                if components.chunks_exact(3).any(|component| {
                    let horizontal = component[1] >> 4;
                    let vertical = component[1] & 0x0f;
                    horizontal == 0
                        || horizontal > 4
                        || vertical == 0
                        || vertical > 4
                        || component[2] > 3
                }) {
                    break;
                }
                let sampling_blocks = components
                    .chunks_exact(3)
                    .map(|component| u16::from(component[1] >> 4) * u16::from(component[1] & 0x0f))
                    .sum::<u16>();
                if sampling_blocks > 10 {
                    break;
                }
                frame_components = components
                    .chunks_exact(3)
                    .map(|component| component[0])
                    .collect();
                frame_quantization_tables = components
                    .chunks_exact(3)
                    .map(|component| usize::from(component[2]))
                    .collect();
                frame_components.sort_unstable();
                frame_components.dedup();
                if frame_components.len() != component_count {
                    break;
                }
                let height = u32::from(u16::from_be_bytes([segment[1], segment[2]]));
                let width = u32::from(u16::from_be_bytes([segment[3], segment[4]]));
                if width == 0 || height == 0 {
                    break;
                }
                dimensions = Some((width, height));
                frame_marker = Some(marker);
            }
            0xc1..=0xc3 | 0xc5..=0xcf if marker != 0xc4 && marker != 0xc8 && marker != 0xcc => {
                return Err(invalid_image(
                    "tool image JPEG uses an unsupported frame coding",
                ));
            }
            0xdb => {
                let mut table_cursor = 0;
                while table_cursor < segment.len() {
                    let Some(&table_info) = segment.get(table_cursor) else {
                        break;
                    };
                    let table_bytes = match table_info >> 4 {
                        0 => 64,
                        _ => break,
                    };
                    if table_info & 0x0f > 3
                        || table_cursor.saturating_add(1 + table_bytes) > segment.len()
                    {
                        break;
                    }
                    let values = &segment[table_cursor + 1..table_cursor + 1 + table_bytes];
                    if values.contains(&0) {
                        break;
                    }
                    quantization_tables[usize::from(table_info & 0x0f)] = true;
                    table_cursor += 1 + table_bytes;
                }
                if table_cursor != segment.len() || segment.is_empty() {
                    break;
                }
            }
            0xc4 => {
                let mut table_cursor = 0;
                while table_cursor < segment.len() {
                    let Some(&table_info) = segment.get(table_cursor) else {
                        break;
                    };
                    if table_info >> 4 > 1 || table_info & 0x0f > 1 {
                        break;
                    }
                    let counts_start = table_cursor.saturating_add(1);
                    let Some(counts) = segment.get(counts_start..counts_start.saturating_add(16))
                    else {
                        break;
                    };
                    let symbol_count = counts
                        .iter()
                        .map(|count| usize::from(*count))
                        .sum::<usize>();
                    if symbol_count == 0 || symbol_count > 256 {
                        break;
                    }
                    let mut available_codes = 1_i32;
                    let mut canonical = true;
                    for count in counts {
                        available_codes = available_codes
                            .saturating_mul(2)
                            .saturating_sub(i32::from(*count));
                        if available_codes < 0 {
                            canonical = false;
                            break;
                        }
                    }
                    if !canonical {
                        break;
                    }
                    let symbols_start = counts_start.saturating_add(16);
                    table_cursor = symbols_start.saturating_add(symbol_count);
                    let Some(symbols) = segment.get(symbols_start..table_cursor) else {
                        break;
                    };
                    let valid_symbols = if table_info >> 4 == 0 {
                        symbols.iter().all(|symbol| *symbol <= 11)
                    } else {
                        symbols.iter().all(|symbol| {
                            let run = symbol >> 4;
                            let size = symbol & 0x0f;
                            size <= 10 && (size != 0 || matches!(run, 0 | 15))
                        })
                    };
                    if !valid_symbols || available_codes == 0 {
                        break;
                    }
                    let table_id = usize::from(table_info & 0x0f);
                    if table_info >> 4 == 0 {
                        dc_huffman_tables[table_id] = true;
                    } else {
                        ac_huffman_tables[table_id] = true;
                    }
                }
                if table_cursor != segment.len() || segment.is_empty() {
                    break;
                }
            }
            0xda => {
                if dimensions.is_none()
                    || frame_quantization_tables
                        .iter()
                        .any(|table| !quantization_tables[*table])
                {
                    break;
                }
                let Some(&component_count) = segment.first() else {
                    break;
                };
                let component_count = usize::from(component_count);
                if component_count == 0
                    || component_count > 4
                    || component_count > frame_components.len()
                    || segment.len() != 4 + component_count * 2
                {
                    break;
                }
                let scan_component_bytes = &segment[1..1 + component_count * 2];
                let mut scan_components = scan_component_bytes
                    .chunks_exact(2)
                    .map(|component| component[0])
                    .collect::<Vec<_>>();
                if scan_components
                    .iter()
                    .any(|component| !frame_components.contains(component))
                {
                    break;
                }
                scan_components.sort_unstable();
                scan_components.dedup();
                if scan_components.len() != component_count {
                    break;
                }
                if scan_components
                    .iter()
                    .any(|component| scanned_components.contains(component))
                {
                    break;
                }
                for component in &scan_components {
                    scanned_components.push(*component);
                }
                let spectral_start = segment[1 + component_count * 2];
                let spectral_end = segment[2 + component_count * 2];
                let approximation = segment[3 + component_count * 2];
                if spectral_start > spectral_end || spectral_end > 63 {
                    break;
                }
                if frame_marker == Some(0xc0)
                    && (spectral_start != 0 || spectral_end != 63 || approximation != 0)
                {
                    break;
                }
                if scan_component_bytes.chunks_exact(2).any(|component| {
                    let selector = component[1];
                    let dc_table = usize::from(selector >> 4);
                    let ac_table = usize::from(selector & 0x0f);
                    dc_table > 1
                        || ac_table > 1
                        || (spectral_start == 0 && !dc_huffman_tables[dc_table])
                        || (spectral_end > 0 && !ac_huffman_tables[ac_table])
                }) {
                    break;
                }
                saw_scan = true;
                in_scan = true;
                current_scan_has_data = false;
                expected_restart = 0;
            }
            0xdd if segment.len() == 2 => {
                restart_interval = u16::from_be_bytes([segment[0], segment[1]]);
            }
            0xe0..=0xef | 0xfe => {}
            _ => {
                return Err(invalid_image(format!(
                    "tool image JPEG contains unsupported marker 0xff{marker:02x}"
                )));
            }
        }
        cursor = cursor.saturating_add(length);
    }
    Err(invalid_image(
        "tool image declares `image/jpeg` but is truncated or has no complete scan",
    ))
}

fn resize_to_fit(image: &DynamicImage, max_dimension: u32) -> DynamicImage {
    if image.width() <= max_dimension && image.height() <= max_dimension {
        image.clone()
    } else {
        image.resize(max_dimension, max_dimension, FilterType::Triangle)
    }
}

fn encode_image(image: &DynamicImage) -> StoreResult<Vec<u8>> {
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, ImageFormat::Png)
        .map_err(|error| invalid_image(format!("tool image could not be encoded: {error}")))?;
    Ok(encoded.into_inner())
}

fn invalid_image(message: impl Into<String>) -> HaiderError {
    store_error(ErrorCode::InvalidArgument, message, false)
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
fn cleanup_temporary(
    temporary: &mut TemporaryPath,
    parent: &Path,
    policy: haider_platform::SyncPolicy,
) -> StoreResult<()> {
    let remove_result = temporary.remove();
    // Temp unlink and final hard-link publication share this directory boundary.
    let sync_result = sync_directory(parent, policy);
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

fn sync_file(file: &File, path: &Path, policy: haider_platform::SyncPolicy) -> StoreResult<()> {
    #[cfg(test)]
    if intercept_sync_for_test(path, policy, CasSyncTarget::File) {
        return Ok(());
    }
    haider_platform::fs::sync_file(file, policy).map_err(|error| io_error("sync file", path, error))
}

/// Fsyncs a directory so its entries (hard links, new subdirectories) survive a
/// crash.
fn sync_directory(path: &Path, policy: haider_platform::SyncPolicy) -> StoreResult<()> {
    #[cfg(test)]
    if intercept_sync_for_test(path, policy, CasSyncTarget::Directory) {
        return Ok(());
    }
    haider_platform::fs::sync_directory(path, policy)
        .map_err(|error| io_error("sync directory", path, error))
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> HaiderError {
    let code = match error.kind() {
        ErrorKind::StorageFull => ErrorCode::StoreFull,
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => ErrorCode::StoreReadOnly,
        _ => ErrorCode::StoreUnavailable,
    };
    store_error(code, format!("{action} {}: {error}", path.display()), true)
}
