//! Same-filesystem immutable staging and strict archive verification.

use super::UpdateError;
use super::discovery::{ReleaseSelection, UpdateTransport};
use super::members::{ALL_BUNDLE_MEMBERS, BUNDLE_MEMBERS, BundleMember};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 16 * 1024;
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TRAILING_ARCHIVE_BYTES: usize = 64 * 1024;
const XZ_MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];
const TAR: &str = "/usr/bin/tar";
#[cfg(target_os = "macos")]
const XATTR: &str = "/usr/bin/xattr";
#[cfg(target_os = "macos")]
const CODESIGN: &str = "/usr/bin/codesign";

pub trait StageVerifier: Sync {
    fn remove_quarantine(&self, path: &Path) -> Result<(), UpdateError>;
    fn sign(&self, path: &Path) -> Result<(), UpdateError>;
    fn verify_signature(&self, path: &Path) -> Result<(), UpdateError>;
    fn smoke_binary(
        &self,
        path: &Path,
        member: BundleMember,
        target: &str,
    ) -> Result<(), UpdateError>;
}

pub struct SystemStageVerifier;

impl StageVerifier for SystemStageVerifier {
    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
    fn sign(&self, path: &Path) -> Result<(), UpdateError> {
        // Preserve the release's Developer ID signature and notarized bytes.
        // Locally built unsigned fixtures still need an ad-hoc signature, but
        // omitting --force ensures an invalid existing signature is rejected
        // instead of silently replacing its identity with an ad-hoc one.
        if self.verify_signature(path).is_ok() {
            return Ok(());
        }
        command_success(
            Command::new(CODESIGN)
                .args(["--sign", "-", "--timestamp=none"])
                .arg(path),
            "ad-hoc sign unsigned staged binary",
        )
    }

    #[cfg(target_os = "macos")]
    fn verify_signature(&self, path: &Path) -> Result<(), UpdateError> {
        command_success(
            Command::new(CODESIGN)
                .args(["--verify", "--strict"])
                .arg(path),
            "verify staged signature",
        )
    }

    // Linux/Windows packages currently carry no updater-managed OS signature
    // authority. Their installer path still requires exact member hashes,
    // executable version identity and the real offline CLI self-test.
    #[cfg(not(target_os = "macos"))]
    fn remove_quarantine(&self, _path: &Path) -> Result<(), UpdateError> {
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn sign(&self, _path: &Path) -> Result<(), UpdateError> {
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn verify_signature(&self, _path: &Path) -> Result<(), UpdateError> {
        Ok(())
    }

    fn smoke_binary(
        &self,
        path: &Path,
        member: BundleMember,
        target: &str,
    ) -> Result<(), UpdateError> {
        let output = bounded_command_output(
            Command::new(path).arg("--version"),
            4096,
            "smoke staged executable version",
        )?;
        let name = member.name();
        if output != format!("{name} {target}\n").as_bytes() {
            return Err(UpdateError::Refused(format!(
                "staged {name} reported an unexpected version; expected {target}"
            )));
        }
        if member == BundleMember::Cli {
            let self_test = bounded_command_output(
                Command::new(path).arg("self-test"),
                64 * 1024,
                "self-test staged haider",
            )?;
            let report: serde_json::Value =
                serde_json::from_slice(&self_test).map_err(|error| {
                    UpdateError::Refused(format!("staged haider self-test was not JSON: {error}"))
                })?;
            if report.get("schema").and_then(serde_json::Value::as_str)
                != Some("haider.selftest.v0")
                || report.get("version").and_then(serde_json::Value::as_str) != Some(target)
                || report.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
            {
                return Err(UpdateError::Refused(
                    "staged haider self-test did not report exact versioned success".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Capability produced only after transport, archive, signing, and smoke
/// verification. Commit consumes this type, so partial or SHA-failed input
/// cannot reach canonical installation paths by construction.
pub struct VerifiedStagedPair {
    _stage: StageDirectory,
    binaries: Vec<StagedBinary>,
    version: String,
    source_digest: String,
}

struct StagedBinary {
    member: BundleMember,
    path: PathBuf,
    digest: String,
}

impl VerifiedStagedPair {
    pub fn path(&self, member: BundleMember) -> &Path {
        &self.binary(member).path
    }

    pub fn digest(&self, member: BundleMember) -> &str {
        &self.binary(member).digest
    }

    fn binary(&self, member: BundleMember) -> &StagedBinary {
        self.binaries
            .iter()
            .find(|binary| binary.member == member)
            .unwrap_or_else(|| unreachable!("requested member is absent from this verified bundle"))
    }

    pub fn members(&self) -> impl Iterator<Item = BundleMember> + '_ {
        self.binaries.iter().map(|binary| binary.member)
    }

    #[cfg(test)]
    pub fn haider_path(&self) -> &Path {
        self.path(BundleMember::Cli)
    }

    #[cfg(test)]
    pub fn haiderd_path(&self) -> &Path {
        self.path(BundleMember::Daemon)
    }

    #[cfg(test)]
    pub fn haider_digest(&self) -> &str {
        self.digest(BundleMember::Cli)
    }

    #[cfg(test)]
    pub fn haiderd_digest(&self) -> &str {
        self.digest(BundleMember::Daemon)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    /// Revalidates every member immediately before transaction entry.
    pub fn verify_immutable(&self) -> Result<(), UpdateError> {
        parallel_members(&self.members().collect::<Vec<_>>(), |member| {
            let path = self.path(member);
            let name = member.name();
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| UpdateError::io("inspect staged binary", error))?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || !haider_platform::metadata_is_current_user(&metadata)
                || haider_platform::metadata_mode(&metadata) & 0o777 != 0o500
            {
                return Err(UpdateError::Refused(format!(
                    "verified staged `{name}` is no longer immutable"
                )));
            }
            if sha256_file(path)? != self.digest(member) {
                return Err(UpdateError::Refused(format!(
                    "verified staged `{name}` digest changed before commit"
                )));
            }
            Ok(())
        })?;
        Ok(())
    }
}

/// Only private, distinct member paths may be processed concurrently. Join all
/// workers before propagating any error so staging cleanup cannot race a writer.
/// Results retain publication order; transaction writes remain serial.
fn parallel_members<T: Send>(
    members: &[BundleMember],
    operation: impl Fn(BundleMember) -> Result<T, UpdateError> + Sync,
) -> Result<Vec<T>, UpdateError> {
    std::thread::scope(|scope| {
        let workers: Vec<_> = members
            .iter()
            .map(|&member| {
                let operation = &operation;
                scope.spawn(move || operation(member))
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| {
                worker.join().unwrap_or_else(|_| {
                    Err(UpdateError::Internal(
                        "bundle verification worker panicked".into(),
                    ))
                })
            })
            .collect();
        results.into_iter().collect()
    })
}

fn freeze_binaries(
    directory: &Path,
    members: &[BundleMember],
) -> Result<Vec<StagedBinary>, UpdateError> {
    let binaries = parallel_members(members, |member| {
        let path = directory.join(member.file_name());
        // Hold a writable flush handle before setting the immutable/read-only
        // attribute; Windows FlushFileBuffers requires write access.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| UpdateError::io("open staged binary for durability", error))?;
        haider_platform::set_mode(&path, 0o500)
            .map_err(|error| UpdateError::io("protect staged binary", error))?;
        file.sync_all()
            .map_err(|error| UpdateError::io("fsync staged binary", error))?;
        drop(file);
        let digest = sha256_file(&path)?;
        Ok(StagedBinary {
            member,
            path,
            digest,
        })
    })?;
    sync_dir(directory)?;
    Ok(binaries)
}

/// Test-only fixture capability; production always checks transport and signatures.
#[cfg(test)]
#[allow(dead_code)]
pub fn verified_pair_for_test(
    install_dir: &Path,
    haider_source: &Path,
    haider_tui_source: &Path,
    haiderd_source: &Path,
    version: &str,
) -> Result<VerifiedStagedPair, UpdateError> {
    verified_bundle_for_test(
        install_dir,
        &[
            (BundleMember::Daemon, haiderd_source),
            (BundleMember::Tui, haider_tui_source),
            (BundleMember::Cli, haider_source),
        ],
        version,
    )
}

#[cfg(test)]
#[allow(dead_code)]
pub fn verified_bundle_for_test(
    install_dir: &Path,
    sources: &[(BundleMember, &Path)],
    version: &str,
) -> Result<VerifiedStagedPair, UpdateError> {
    let stage = StageDirectory {
        path: create_stage_dir(install_dir)?,
    };
    let binary_dir = stage.path.join("verified-fixture");
    fs::create_dir(&binary_dir)
        .map_err(|error| UpdateError::io("create verified fixture directory", error))?;
    let members: Vec<_> = ALL_BUNDLE_MEMBERS
        .into_iter()
        .filter(|member| sources.iter().any(|(candidate, _)| candidate == member))
        .collect();
    for member in &members {
        let (_, source) = sources
            .iter()
            .find(|(candidate, _)| candidate == member)
            .ok_or_else(|| UpdateError::Internal("fixture member lacks source".into()))?;
        fs::copy(source, binary_dir.join(member.file_name()))
            .map_err(|error| UpdateError::io("copy verified fixture binary", error))?;
    }
    let binaries = freeze_binaries(&binary_dir, &members)?;
    Ok(VerifiedStagedPair {
        _stage: stage,
        source_digest: binaries
            .first()
            .map(|binary| binary.digest.clone())
            .unwrap_or_default(),
        binaries,
        version: version.to_owned(),
    })
}

struct StageDirectory {
    path: PathBuf,
}

impl Drop for StageDirectory {
    fn drop(&mut self) {
        #[cfg(windows)]
        clear_private_stage_readonly(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(windows)]
fn clear_private_stage_readonly(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            clear_private_stage_readonly(&path);
        } else if metadata.is_file() {
            let _ = haider_platform::set_mode(&path, 0o700);
        }
    }
}

/// Downloads and verifies an update without opening either canonical binary.
///
/// MUTATION SAFETY: all writes are confined to a new owner-only directory
/// beneath `install_dir`; every runtime failure removes that directory when
/// the returned capability is dropped. Canonical executable paths
/// are never accepted by this API.
pub fn stage_release<T: UpdateTransport, V: StageVerifier>(
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
        "haider-v{}-{}-split",
        selection.version,
        super::discovery::compiled_target()?
    );
    if selection.archive_name != format!("{top}.tar.xz")
        || selection.checksum_name != format!("{top}.tar.xz.sha256")
    {
        return Err(UpdateError::Refused(
            "update selection is not an exact split-bundle asset".into(),
        ));
    }
    let extracted = root.join(&top);
    fs::create_dir(&extracted)
        .map_err(|error| UpdateError::io("create extracted staging directory", error))?;
    haider_platform::set_mode(&extracted, 0o700)
        .map_err(|error| UpdateError::io("protect extracted staging directory", error))?;
    extract_strict(&archive, root, &top)?;

    let binaries = verify_and_freeze(
        verifier,
        &extracted,
        &selection.version.to_string(),
        &BUNDLE_MEMBERS,
    )?;
    sync_dir(root)?;
    let pair = VerifiedStagedPair {
        _stage: stage,
        binaries,
        version: selection.version.to_string(),
        source_digest: actual_digest,
    };
    pair.verify_immutable()?;
    Ok(pair)
}

/// Stages the exact executable bytes embedded in a compatibility entrypoint.
/// The canonical daemon is copied into the same owner-only staging directory,
/// then every member receives the normal signature, exact-version and CLI
/// self-test checks before the immutable commit capability can be constructed.
/// No canonical path or update lock is modified here.
pub fn stage_embedded_bundle(
    thin: &[u8],
    payload: &[u8],
    daemon_source: &Path,
    install_dir: &Path,
    version: &str,
) -> Result<VerifiedStagedPair, UpdateError> {
    super::discovery::compiled_target()?;
    super::discovery::SemVersion::parse(version).map_err(UpdateError::Refused)?;
    let stage = StageDirectory {
        path: create_stage_dir(install_dir)?,
    };
    let extracted = stage.path.join("embedded-bundle");
    fs::create_dir(&extracted)
        .map_err(|error| UpdateError::io("create embedded staging directory", error))?;
    haider_platform::set_mode(&extracted, 0o700)
        .map_err(|error| UpdateError::io("protect embedded staging directory", error))?;
    let mut source_manifest = String::new();
    for member in BUNDLE_MEMBERS {
        let destination = extracted.join(member.file_name());
        create_private_file(&destination)?;
        match member {
            BundleMember::Cli => write_embedded_binary(&destination, thin)?,
            BundleMember::Tui => write_embedded_binary(&destination, payload)?,
            BundleMember::Daemon => {
                let metadata = fs::symlink_metadata(daemon_source).map_err(|error| {
                    UpdateError::io("inspect embedded bundle daemon source", error)
                })?;
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.len() == 0
                    || metadata.len() > MAX_BINARY_BYTES
                    || !haider_platform::metadata_is_current_user(&metadata)
                {
                    return Err(UpdateError::Refused(
                        "embedded bundle daemon is not a trusted bounded regular file".into(),
                    ));
                }
                fs::copy(daemon_source, &destination)
                    .map_err(|error| UpdateError::io("copy embedded bundle daemon", error))?;
            }
            BundleMember::WaylandPortal => {
                unreachable!("embedded migration has only required members")
            }
        }
        haider_platform::set_mode(&destination, 0o700)
            .map_err(|error| UpdateError::io("make embedded stage executable", error))?;
        use std::fmt::Write as _;
        writeln!(
            &mut source_manifest,
            "{} {}",
            member.name(),
            sha256_file(&destination)?
        )
        .map_err(|error| {
            UpdateError::Internal(format!("encode embedded source manifest: {error}"))
        })?;
    }
    let manifest = stage.path.join("embedded-sources.sha256");
    create_private_file(&manifest)?;
    fs::write(&manifest, source_manifest.as_bytes())
        .map_err(|error| UpdateError::io("write embedded source manifest", error))?;
    let source_digest = sha256_file(&manifest)?;
    let binaries = verify_and_freeze(&SystemStageVerifier, &extracted, version, &BUNDLE_MEMBERS)?;
    sync_dir(&stage.path)?;
    let bundle = VerifiedStagedPair {
        _stage: stage,
        binaries,
        version: version.to_owned(),
        source_digest,
    };
    bundle.verify_immutable()?;
    Ok(bundle)
}

/// Copies an unpacked, checksum-verified installer bundle into private staging.
/// Directory installers and the network updater share all subsequent member
/// verification, publication, rollback and recovery code.
pub fn stage_directory_bundle(
    source: &Path,
    install_dir: &Path,
    version: &str,
) -> Result<VerifiedStagedPair, UpdateError> {
    super::discovery::SemVersion::parse(version).map_err(UpdateError::Refused)?;
    let stage = StageDirectory {
        path: create_stage_dir(install_dir)?,
    };
    let extracted = stage.path.join("directory-bundle");
    fs::create_dir(&extracted)
        .map_err(|error| UpdateError::io("create directory-bundle staging", error))?;
    haider_platform::set_mode(&extracted, 0o700)
        .map_err(|error| UpdateError::io("protect directory-bundle staging", error))?;
    let portal = source.join(BundleMember::WaylandPortal.file_name());
    let has_portal = match fs::symlink_metadata(&portal) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(UpdateError::io("inspect optional portal source", error)),
    };
    let members: Vec<_> = ALL_BUNDLE_MEMBERS
        .into_iter()
        .filter(|member| *member != BundleMember::WaylandPortal || has_portal)
        .collect();
    let source_entries = parallel_members(&members, |member| {
        let original = source.join(member.file_name());
        let metadata = fs::symlink_metadata(&original)
            .map_err(|error| UpdateError::io("inspect installer source executable", error))?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() == 0
            || metadata.len() > MAX_BINARY_BYTES
        {
            return Err(UpdateError::Refused(format!(
                "installer source {} is not a bounded regular executable",
                member.file_name()
            )));
        }
        let source_digest = sha256_file(&original)?;
        let destination = extracted.join(member.file_name());
        create_private_file(&destination)?;
        fs::copy(&original, &destination)
            .map_err(|error| UpdateError::io("copy installer source executable", error))?;
        if sha256_file(&destination)? != source_digest {
            return Err(UpdateError::Refused(
                "installer source changed while staging".into(),
            ));
        }
        haider_platform::set_mode(&destination, 0o700)
            .map_err(|error| UpdateError::io("make directory stage executable", error))?;
        Ok(format!("{} {source_digest}\n", member.name()))
    })?;
    let source_manifest = source_entries.concat();
    let manifest = stage.path.join("directory-sources.sha256");
    create_private_file(&manifest)?;
    fs::write(&manifest, source_manifest.as_bytes())
        .map_err(|error| UpdateError::io("write directory source manifest", error))?;
    let source_digest = sha256_file(&manifest)?;
    let binaries = verify_and_freeze(&SystemStageVerifier, &extracted, version, &members)?;
    sync_dir(&stage.path)?;
    let bundle = VerifiedStagedPair {
        _stage: stage,
        binaries,
        version: version.to_owned(),
        source_digest,
    };
    bundle.verify_immutable()?;
    Ok(bundle)
}

fn write_embedded_binary(path: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_BINARY_BYTES {
        return Err(UpdateError::Refused(
            "embedded executable is empty or oversized".into(),
        ));
    }
    fs::write(path, bytes).map_err(|error| UpdateError::io("write embedded executable", error))
}

fn verify_and_freeze<V: StageVerifier>(
    verifier: &V,
    directory: &Path,
    version: &str,
    members: &[BundleMember],
) -> Result<Vec<StagedBinary>, UpdateError> {
    // Sign the entire bundle before any smoke: the CLI self-test can execute
    // the staged daemon sibling, so every dependency must already be verified.
    parallel_members(members, |member| {
        let binary = directory.join(member.file_name());
        verifier.remove_quarantine(&binary)?;
        verifier.sign(&binary)?;
        verifier.verify_signature(&binary)
    })?;
    parallel_members(members, |member| {
        verifier.smoke_binary(&directory.join(member.file_name()), member, version)
    })?;
    freeze_binaries(directory, members)
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
        haider_platform::configure_directory_mode(&mut builder, 0o700);
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
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    haider_platform::configure_file_mode(&mut options, 0o600);
    options
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| UpdateError::io("create private partial download", error))?;
    Ok(())
}

pub fn parse_checksum(path: &Path, archive_name: &str) -> Result<String, UpdateError> {
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

pub fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    use sha2::{Digest as _, Sha256};
    let mut file =
        File::open(path).map_err(|error| UpdateError::io("open file for SHA-256", error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| UpdateError::io("read file for SHA-256", error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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
    let expected: BTreeSet<_> = std::iter::once(expected_dir.clone())
        .chain(BUNDLE_MEMBERS.map(|member| format!("{top}/{}", member.name())))
        .collect();
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
            // Exact allowlist membership above already excludes every other path.
            let destination = root.join(&name);
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
            "archive is missing the top directory or a required bundle executable".into(),
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
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    haider_platform::configure_file_mode(&mut options, 0o700);
    let mut file = options
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

#[cfg(target_os = "macos")]
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

pub fn bounded_command_output(
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

pub fn sync_dir(path: &Path) -> Result<(), UpdateError> {
    haider_platform::sync_directory(path).map_err(|error| UpdateError::io("fsync directory", error))
}
