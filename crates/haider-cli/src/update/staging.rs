//! Same-filesystem immutable staging and strict archive verification.

use super::UpdateError;
use super::discovery::{ReleaseSelection, UpdateTransport};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 16 * 1024;
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TRAILING_ARCHIVE_BYTES: usize = 64 * 1024;
const XZ_MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];
const TAR: &str = "/usr/bin/tar";
const SHASUM: &str = "/usr/bin/shasum";
const XATTR: &str = "/usr/bin/xattr";
const CODESIGN: &str = "/usr/bin/codesign";

pub(crate) trait StageVerifier {
    fn remove_quarantine(&self, path: &Path) -> Result<(), UpdateError>;
    fn sign(&self, path: &Path) -> Result<(), UpdateError>;
    fn verify_signature(&self, path: &Path) -> Result<(), UpdateError>;
    fn smoke_haider(&self, path: &Path, target: &str) -> Result<(), UpdateError>;
    fn smoke_haiderd(&self, path: &Path, target: &str) -> Result<(), UpdateError>;
}

pub(crate) struct SystemStageVerifier;

impl StageVerifier for SystemStageVerifier {
    fn remove_quarantine(&self, path: &Path) -> Result<(), UpdateError> {
        let output =
            bounded_command_output(Command::new(XATTR).arg(path), 64 * 1024, "list xattrs")?;
        let attributes = std::str::from_utf8(&output)
            .map_err(|_| UpdateError::Refused("xattr output was not UTF-8".into()))?;
        if attributes
            .lines()
            .any(|attribute| attribute.trim() == "com.apple.quarantine")
        {
            command_success(
                Command::new(XATTR)
                    .args(["-d", "com.apple.quarantine"])
                    .arg(path),
                "remove com.apple.quarantine",
            )?;
        }
        Ok(())
    }

    fn sign(&self, path: &Path) -> Result<(), UpdateError> {
        command_success(
            Command::new(CODESIGN)
                .args(["--force", "--sign", "-", "--timestamp=none"])
                .arg(path),
            "ad-hoc sign staged binary",
        )
    }

    fn verify_signature(&self, path: &Path) -> Result<(), UpdateError> {
        command_success(
            Command::new(CODESIGN)
                .args(["--verify", "--strict"])
                .arg(path),
            "verify staged signature",
        )
    }

    fn smoke_haider(&self, path: &Path, target: &str) -> Result<(), UpdateError> {
        let output = bounded_command_output(
            Command::new(path).arg("--version"),
            4096,
            "smoke staged haider",
        )?;
        if output != format!("haider {target}\n").as_bytes() {
            return Err(UpdateError::Refused(format!(
                "staged haider reported an unexpected version; expected {target}"
            )));
        }
        let self_test = bounded_command_output(
            Command::new(path).arg("self-test"),
            64 * 1024,
            "self-test staged haider",
        )?;
        let report: serde_json::Value = serde_json::from_slice(&self_test).map_err(|error| {
            UpdateError::Refused(format!("staged haider self-test was not JSON: {error}"))
        })?;
        if report.get("schema").and_then(serde_json::Value::as_str) != Some("haider.selftest.v0")
            || report.get("version").and_then(serde_json::Value::as_str) != Some(target)
            || report.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
        {
            return Err(UpdateError::Refused(
                "staged haider self-test did not report exact versioned success".into(),
            ));
        }
        Ok(())
    }

    fn smoke_haiderd(&self, path: &Path, target: &str) -> Result<(), UpdateError> {
        let output = bounded_command_output(
            Command::new(path).arg("--version"),
            4096,
            "smoke staged haiderd",
        )?;
        if output != format!("haiderd {target}\n").as_bytes() {
            return Err(UpdateError::Refused(format!(
                "staged haiderd reported an unexpected version; expected {target}"
            )));
        }
        Ok(())
    }
}

/// Capability produced only after transport, archive, signing, and smoke
/// verification. Commit consumes this type, so partial or SHA-failed input
/// cannot reach canonical installation paths by construction.
pub(crate) struct VerifiedStagedPair {
    _stage: StageDirectory,
    haider: PathBuf,
    haiderd: PathBuf,
    version: String,
    haider_digest: String,
    haiderd_digest: String,
    source_digest: String,
}

impl VerifiedStagedPair {
    pub fn haider_path(&self) -> &Path {
        &self.haider
    }

    pub fn haiderd_path(&self) -> &Path {
        &self.haiderd
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn haider_digest(&self) -> &str {
        &self.haider_digest
    }

    pub fn haiderd_digest(&self) -> &str {
        &self.haiderd_digest
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    /// Revalidates the opaque capability immediately before transaction entry.
    pub fn verify_immutable(&self) -> Result<(), UpdateError> {
        for (path, digest, name) in [
            (&self.haider, &self.haider_digest, "haider"),
            (&self.haiderd, &self.haiderd_digest, "haiderd"),
        ] {
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| UpdateError::io("inspect staged binary", error))?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != haider_client::effective_uid()
                || metadata.permissions().mode() & 0o777 != 0o500
            {
                return Err(UpdateError::Refused(format!(
                    "verified staged `{name}` is no longer immutable"
                )));
            }
            if sha256_file(path)? != *digest {
                return Err(UpdateError::Refused(format!(
                    "verified staged `{name}` digest changed before commit"
                )));
            }
        }
        Ok(())
    }
}

/// Builds the same opaque capability from already-verified fixture binaries.
/// Test-only: production has no bypass around transport/signature/smoke.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn verified_pair_for_test(
    install_dir: &Path,
    haider_source: &Path,
    haiderd_source: &Path,
    version: &str,
) -> Result<VerifiedStagedPair, UpdateError> {
    let stage = StageDirectory {
        path: create_stage_dir(install_dir)?,
    };
    let binary_dir = stage.path.join("verified-fixture");
    fs::create_dir(&binary_dir)
        .map_err(|error| UpdateError::io("create verified fixture directory", error))?;
    let haider = binary_dir.join("haider");
    let haiderd = binary_dir.join("haiderd");
    fs::copy(haider_source, &haider)
        .map_err(|error| UpdateError::io("copy verified fixture haider", error))?;
    fs::copy(haiderd_source, &haiderd)
        .map_err(|error| UpdateError::io("copy verified fixture haiderd", error))?;
    for binary in [&haider, &haiderd] {
        fs::set_permissions(binary, fs::Permissions::from_mode(0o500))
            .map_err(|error| UpdateError::io("chmod verified fixture binary", error))?;
        File::open(binary)
            .and_then(|file| file.sync_all())
            .map_err(|error| UpdateError::io("fsync verified fixture binary", error))?;
    }
    sync_dir(&binary_dir)?;
    let haider_digest = sha256_file(&haider)?;
    let haiderd_digest = sha256_file(&haiderd)?;
    Ok(VerifiedStagedPair {
        _stage: stage,
        haider,
        haiderd,
        version: version.to_owned(),
        source_digest: haider_digest.clone(),
        haider_digest,
        haiderd_digest,
    })
}

struct StageDirectory {
    path: PathBuf,
}

impl Drop for StageDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Downloads and verifies an update without opening either canonical binary.
///
/// MUTATION SAFETY: all writes are confined to a new owner-only directory
/// beneath `install_dir`; every runtime failure removes that directory when
/// the returned capability is dropped. Canonical `haider`/`haiderd` paths
/// are never accepted by this API.
pub(crate) fn stage_release<T: UpdateTransport, V: StageVerifier>(
    transport: &mut T,
    verifier: &V,
    install_dir: &Path,
    selection: &ReleaseSelection,
) -> Result<VerifiedStagedPair, UpdateError> {
    let stage = StageDirectory {
        path: create_stage_dir(install_dir)?,
    };
    let root = &stage.path;
    let archive = root.join(format!("{}.part", selection.archive_name));
    let checksum = root.join(format!("{}.part", selection.checksum_name));
    create_private_file(&archive)?;
    create_private_file(&checksum)?;

    transport.download(&selection.archive_url, &archive, MAX_ARCHIVE_BYTES)?;
    transport.download(&selection.checksum_url, &checksum, MAX_CHECKSUM_BYTES)?;
    let expected_digest = parse_checksum(&checksum, &selection.archive_name)?;
    let actual_digest = sha256_file(&archive)?;
    if expected_digest != actual_digest {
        return Err(UpdateError::Refused(
            "release archive SHA-256 does not match its checksum".into(),
        ));
    }

    let top = format!(
        "haider-v{}-{}",
        selection.version,
        super::discovery::compiled_target()?
    );
    let extracted = root.join(&top);
    fs::create_dir(&extracted)
        .map_err(|error| UpdateError::io("create extracted staging directory", error))?;
    fs::set_permissions(&extracted, fs::Permissions::from_mode(0o700))
        .map_err(|error| UpdateError::io("protect extracted staging directory", error))?;
    extract_strict(&archive, root, &top)?;

    let haider = extracted.join("haider");
    let haiderd = extracted.join("haiderd");
    for binary in [&haider, &haiderd] {
        verifier.remove_quarantine(binary)?;
        verifier.sign(binary)?;
        verifier.verify_signature(binary)?;
    }
    verifier.smoke_haider(&haider, &selection.version.to_string())?;
    verifier.smoke_haiderd(&haiderd, &selection.version.to_string())?;

    for binary in [&haider, &haiderd] {
        fs::set_permissions(binary, fs::Permissions::from_mode(0o500))
            .map_err(|error| UpdateError::io("protect staged binary", error))?;
        File::open(binary)
            .and_then(|file| file.sync_all())
            .map_err(|error| UpdateError::io("fsync staged binary", error))?;
    }
    sync_dir(&extracted)?;
    sync_dir(root)?;
    let haider_digest = sha256_file(&haider)?;
    let haiderd_digest = sha256_file(&haiderd)?;
    let pair = VerifiedStagedPair {
        _stage: stage,
        haider,
        haiderd,
        version: selection.version.to_string(),
        haider_digest,
        haiderd_digest,
        source_digest: actual_digest,
    };
    pair.verify_immutable()?;
    Ok(pair)
}

fn create_stage_dir(install_dir: &Path) -> Result<PathBuf, UpdateError> {
    for attempt in 0..32_u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = install_dir.join(format!(
            ".haider-update-stage-{}-{nanos}-{attempt}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(UpdateError::io("create update staging directory", error)),
        }
    }
    Err(UpdateError::Internal(
        "could not allocate a unique update staging directory".into(),
    ))
}

fn create_private_file(path: &Path) -> Result<(), UpdateError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| UpdateError::io("create private partial download", error))?;
    Ok(())
}

pub(crate) fn parse_checksum(path: &Path, archive_name: &str) -> Result<String, UpdateError> {
    let bytes = fs::read(path).map_err(|error| UpdateError::io("read release checksum", error))?;
    if bytes.len() > usize::try_from(MAX_CHECKSUM_BYTES).unwrap_or(usize::MAX) {
        return Err(UpdateError::Refused("checksum file is oversized".into()));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| UpdateError::Refused("checksum file is not UTF-8".into()))?;
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(UpdateError::Refused(
            "checksum must contain exactly one non-empty line".into(),
        ));
    }
    let fields = lines[0].split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2
        || fields[0].len() != 64
        || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(UpdateError::Refused(
            "checksum must contain exactly one 64-hex digest and filename".into(),
        ));
    }
    let referenced = Path::new(fields[1]);
    if referenced.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) || referenced.file_name().and_then(|name| name.to_str()) != Some(archive_name)
    {
        return Err(UpdateError::Refused(format!(
            "checksum references a different archive than `{archive_name}`"
        )));
    }
    Ok(fields[0].to_ascii_lowercase())
}

pub(crate) fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let output = bounded_command_output(
        Command::new(SHASUM).args(["-a", "256"]).arg(path),
        4096,
        "hash file",
    )?;
    let text = std::str::from_utf8(&output)
        .map_err(|_| UpdateError::Refused("shasum output was not UTF-8".into()))?;
    let fields = text.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 2
        || fields[0].len() != 64
        || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(UpdateError::Refused(
            "shasum returned malformed output".into(),
        ));
    }
    Ok(fields[0].to_ascii_lowercase())
}

fn extract_strict(archive: &Path, root: &Path, top: &str) -> Result<(), UpdateError> {
    let mut magic = [0_u8; 6];
    File::open(archive)
        .and_then(|mut file| file.read_exact(&mut magic))
        .map_err(|error| UpdateError::io("read archive header", error))?;
    if magic != XZ_MAGIC {
        return Err(UpdateError::Refused("release archive is not XZ".into()));
    }
    let mut command = Command::new(TAR);
    command
        .args(["-c", "-f", "-", "--format", "ustar"])
        .arg(format!("@{}", archive.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| UpdateError::io("start strict archive decoder", error))?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        UpdateError::Internal("archive decoder output pipe was not created".into())
    })?;
    let parsed = parse_ustar(&mut stdout, root, top);
    if parsed.is_err() {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| UpdateError::io("wait for strict archive decoder", error))?;
    parsed?;
    if !status.success() {
        return Err(UpdateError::Refused(
            "archive decompression or normalization failed".into(),
        ));
    }
    Ok(())
}

fn parse_ustar(reader: &mut impl Read, root: &Path, top: &str) -> Result<(), UpdateError> {
    let expected_dir = format!("{top}/");
    let expected_haider = format!("{top}/haider");
    let expected_haiderd = format!("{top}/haiderd");
    let expected = BTreeSet::from([
        expected_dir.clone(),
        expected_haider.clone(),
        expected_haiderd.clone(),
    ]);
    let mut seen = BTreeSet::new();
    let mut zero_blocks = 0_u8;

    loop {
        let mut header = [0_u8; 512];
        read_exact_archive(reader, &mut header, "archive header")?;
        if header.iter().all(|byte| *byte == 0) {
            zero_blocks = zero_blocks.saturating_add(1);
            if zero_blocks == 2 {
                break;
            }
            continue;
        }
        if zero_blocks != 0 {
            return Err(UpdateError::Refused(
                "archive has data between its terminal zero blocks".into(),
            ));
        }
        validate_header_checksum(&header)?;
        let name = archive_path(&header)?;
        if !expected.contains(&name) {
            return Err(UpdateError::Refused(format!(
                "archive contains unexpected or unsafe member `{name}`"
            )));
        }
        if !seen.insert(name.clone()) {
            return Err(UpdateError::Refused(format!(
                "archive contains duplicate member `{name}`"
            )));
        }
        if seen.len() > expected.len() {
            return Err(UpdateError::Refused(
                "archive contains too many members".into(),
            ));
        }

        let mode = parse_octal(&header[100..108], "mode")?;
        let size = parse_octal(&header[124..136], "size")?;
        let kind = header[156];
        if name == expected_dir {
            if kind != b'5' || size != 0 {
                return Err(UpdateError::Refused(
                    "archive top-level member is not an empty directory".into(),
                ));
            }
        } else {
            if !matches!(kind, 0 | b'0') {
                return Err(UpdateError::Refused(format!(
                    "archive binary `{name}` is not a regular file"
                )));
            }
            if mode & 0o100 == 0 {
                return Err(UpdateError::Refused(format!(
                    "archive binary `{name}` is not owner-executable"
                )));
            }
            if size == 0 || size > MAX_BINARY_BYTES {
                return Err(UpdateError::Refused(format!(
                    "archive binary `{name}` is empty or oversized"
                )));
            }
            let destination = if name == expected_haider {
                root.join(top).join("haider")
            } else {
                root.join(top).join("haiderd")
            };
            extract_member(reader, &destination, size)?;
        }
        if name == expected_dir {
            skip_padding(reader, size)?;
        } else {
            skip_padding_only(reader, size)?;
        }
    }

    if seen != expected {
        return Err(UpdateError::Refused(
            "archive is missing the top directory or one of the two binaries".into(),
        ));
    }
    let mut trailing = Vec::new();
    reader
        .take(u64::try_from(MAX_TRAILING_ARCHIVE_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut trailing)
        .map_err(|error| UpdateError::io("read archive trailer", error))?;
    if trailing.len() > MAX_TRAILING_ARCHIVE_BYTES || trailing.iter().any(|byte| *byte != 0) {
        return Err(UpdateError::Refused(
            "archive has non-zero or excessive trailing data".into(),
        ));
    }
    sync_dir(&root.join(top))?;
    Ok(())
}

fn extract_member(
    reader: &mut impl Read,
    destination: &Path,
    size: u64,
) -> Result<(), UpdateError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(destination)
        .map_err(|error| UpdateError::io("create staged binary", error))?;
    let mut remaining = size;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining != 0 {
        let amount = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        reader
            .read_exact(&mut buffer[..amount])
            .map_err(|error| UpdateError::io("read staged binary from archive", error))?;
        file.write_all(&buffer[..amount])
            .map_err(|error| UpdateError::io("write staged binary", error))?;
        remaining -= amount as u64;
    }
    file.sync_all()
        .map_err(|error| UpdateError::io("fsync staged binary", error))?;
    Ok(())
}

fn skip_padding(reader: &mut impl Read, size: u64) -> Result<(), UpdateError> {
    if size != 0 {
        let mut sink = io::sink();
        io::copy(&mut reader.take(size), &mut sink)
            .map_err(|error| UpdateError::io("skip archive member", error))?;
    }
    skip_padding_only(reader, size)
}

fn skip_padding_only(reader: &mut impl Read, size: u64) -> Result<(), UpdateError> {
    let padding = (512 - (size % 512)) % 512;
    if padding != 0 {
        let mut bytes = vec![0_u8; usize::try_from(padding).unwrap_or(511)];
        read_exact_archive(reader, &mut bytes, "archive padding")?;
        if bytes.iter().any(|byte| *byte != 0) {
            return Err(UpdateError::Refused(
                "archive member padding is not zero-filled".into(),
            ));
        }
    }
    Ok(())
}

fn validate_header_checksum(header: &[u8; 512]) -> Result<(), UpdateError> {
    let stored = parse_octal(&header[148..156], "header checksum")?;
    let computed = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum::<u64>();
    if stored != computed {
        return Err(UpdateError::Refused(
            "archive member header checksum is invalid".into(),
        ));
    }
    Ok(())
}

fn archive_path(header: &[u8; 512]) -> Result<String, UpdateError> {
    let name = nul_terminated(&header[..100])?;
    let prefix = nul_terminated(&header[345..500])?;
    let path = if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    };
    if path.is_empty()
        || path.starts_with('/')
        || Path::new(&path).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(UpdateError::Refused(
            "archive contains an absolute or traversing path".into(),
        ));
    }
    Ok(path)
}

fn nul_terminated(bytes: &[u8]) -> Result<&str, UpdateError> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if bytes[end..].iter().any(|byte| *byte != 0) {
        return Err(UpdateError::Refused(
            "archive header string has data after NUL".into(),
        ));
    }
    std::str::from_utf8(&bytes[..end])
        .map_err(|_| UpdateError::Refused("archive path is not UTF-8".into()))
}

fn parse_octal(bytes: &[u8], field: &str) -> Result<u64, UpdateError> {
    if bytes.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err(UpdateError::Refused(format!(
            "archive {field} uses unsupported base-256 encoding"
        )));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| UpdateError::Refused(format!("archive {field} is not ASCII")))?;
    let digits = text.trim_matches(['\0', ' ']);
    if digits.is_empty() || !digits.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
        return Err(UpdateError::Refused(format!(
            "archive {field} is not a valid octal number"
        )));
    }
    u64::from_str_radix(digits, 8)
        .map_err(|_| UpdateError::Refused(format!("archive {field} is too large")))
}

fn read_exact_archive(
    reader: &mut impl Read,
    buffer: &mut [u8],
    what: &'static str,
) -> Result<(), UpdateError> {
    reader
        .read_exact(buffer)
        .map_err(|error| UpdateError::io(what, error))
}

fn command_success(command: &mut Command, operation: &'static str) -> Result<(), UpdateError> {
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| UpdateError::io(operation, error))?;
    if !status.success() {
        return Err(UpdateError::Refused(format!("{operation} failed")));
    }
    Ok(())
}

pub(crate) fn bounded_command_output(
    command: &mut Command,
    limit: usize,
    operation: &'static str,
) -> Result<Vec<u8>, UpdateError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| UpdateError::io(operation, error))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| UpdateError::Internal(format!("{operation} output pipe was not created")))?;
    let mut output = Vec::new();
    stdout
        .by_ref()
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut output)
        .map_err(|error| UpdateError::io(operation, error))?;
    if output.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        return Err(UpdateError::Refused(format!(
            "{operation} output exceeded its bound"
        )));
    }
    let status = child
        .wait()
        .map_err(|error| UpdateError::io(operation, error))?;
    if !status.success() {
        return Err(UpdateError::Refused(format!("{operation} failed")));
    }
    Ok(output)
}

pub(crate) fn sync_dir(path: &Path) -> Result<(), UpdateError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| UpdateError::io("fsync directory", error))
}
