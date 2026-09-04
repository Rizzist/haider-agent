#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Installer tests. No test here touches the network: the HTTP half sits
//! behind [`ArchiveSource`], and every case drives the real filesystem half —
//! sink, size check, digest check, screening, extraction, activation, leases —
//! with fixture archives built in a temp directory and hashed locally.

use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use reqwest::Url;
use sha2::Digest as _;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::antigravity_install::{
    ACTIVE_POINTER, ANTIGRAVITY_PINS, ANTIGRAVITY_VERSION, AntigravityInstallError,
    AntigravityInstaller, AntigravityPin, ArchiveSink, ArchiveSource, DOWNLOADS_DIRECTORY,
    EntryKindFault, EntryPathFault, InstallOutcome, LEASES_DIRECTORY, MAX_ARCHIVE_ENTRIES,
    VERSIONS_DIRECTORY, approved_archive_origin, pin_for_host, pin_for_platform,
};

/// Archive-root names the fixture pins expect, matching the Unix release pins.
const EXE: &str = "agy_acp_server.par";
const HELPER: &str = "localharness_external";
/// Fixture argv, matching the linux registry entry so the argv plumbing is
/// exercised on every host.
const TEST_ARGS: &[&str] = &["--uid="];
const EXE_BODY: &[u8] = b"antigravity-acp-executable-fixture";
const HELPER_BODY: &[u8] = b"localharness-external-fixture";
/// A well-formed but wrong digest: 64 lowercase hex characters.
const WRONG_DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// Fixture archives
// ---------------------------------------------------------------------------

enum Entry<'a> {
    Stored { name: &'a str, data: Vec<u8> },
    Deflated { name: &'a str, data: Vec<u8> },
    Symlink { name: &'a str, target: &'a str },
    Directory { name: &'a str },
}

fn build_archive(entries: Vec<Entry<'_>>) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for entry in entries {
        match entry {
            Entry::Stored { name, data } => {
                let options = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Stored)
                    .unix_permissions(0o555);
                writer
                    .start_file(name, options)
                    .expect("start stored entry");
                writer.write_all(&data).expect("write stored entry");
            }
            Entry::Deflated { name, data } => {
                let options = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .unix_permissions(0o555);
                writer
                    .start_file(name, options)
                    .expect("start deflated entry");
                writer.write_all(&data).expect("write deflated entry");
            }
            Entry::Symlink { name, target } => {
                writer
                    .add_symlink(name, target, SimpleFileOptions::default())
                    .expect("add symlink entry");
            }
            Entry::Directory { name } => {
                writer
                    .add_directory(name, SimpleFileOptions::default())
                    .expect("add directory entry");
            }
        }
    }
    writer.finish().expect("finish archive").into_inner()
}

fn valid_archive() -> Vec<u8> {
    build_archive(vec![
        Entry::Stored {
            name: EXE,
            data: EXE_BODY.to_vec(),
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ])
}

/// A valid archive large enough that a transfer can be interrupted part way.
fn large_valid_archive() -> Vec<u8> {
    build_archive(vec![
        Entry::Stored {
            name: EXE,
            data: vec![0x41; 256 * 1024],
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ])
}

/// 16 MiB of zeros deflates to roughly 16 KiB — a ratio near 1000:1 against
/// the pinned archives' measured 2.9:1.
fn compression_bomb_archive() -> Vec<u8> {
    build_archive(vec![
        Entry::Deflated {
            name: EXE,
            data: vec![0_u8; 16 * 1024 * 1024],
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ])
}

/// `ZipWriter` refuses to write two entries under the same name, so the
/// duplicate is forged. A placeholder entry is written whose name has exactly
/// the same byte length as the executable's, then those bytes are rewritten.
/// ZIP records the name in both the local header and the central directory and
/// checksums only file DATA, so a same-length rename leaves every offset,
/// length field and CRC valid — which is precisely the archive a hostile
/// packer would hand us.
fn duplicate_name_archive() -> Vec<u8> {
    const PLACEHOLDER: &str = "zzzzzzzzzzzzzzzzzz";
    assert_eq!(
        PLACEHOLDER.len(),
        EXE.len(),
        "placeholder must be EXE-sized"
    );
    let bytes = build_archive(vec![
        Entry::Stored {
            name: EXE,
            data: b"first-copy".to_vec(),
        },
        Entry::Stored {
            name: PLACEHOLDER,
            data: b"second-copy".to_vec(),
        },
    ]);
    replace_all(&bytes, PLACEHOLDER.as_bytes(), EXE.as_bytes())
}

fn replace_all(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    assert_eq!(needle.len(), replacement.len());
    let mut out = haystack.to_vec();
    let mut index = 0;
    let mut hits = 0;
    while index + needle.len() <= out.len() {
        if &out[index..index + needle.len()] == needle {
            out[index..index + needle.len()].copy_from_slice(replacement);
            hits += 1;
            index += needle.len();
        } else {
            index += 1;
        }
    }
    assert!(
        hits >= 2,
        "expected a local-header hit and a central-directory hit, got {hits}"
    );
    out
}

// ---------------------------------------------------------------------------
// Fixture pins and source
// ---------------------------------------------------------------------------

fn leak_str(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn make_pin(version: &'static str, size: u64, digest: &'static str) -> &'static AntigravityPin {
    Box::leak(Box::new(AntigravityPin::for_test(
        version, size, digest, EXE, HELPER, TEST_ARGS,
    )))
}

/// A pin whose size and digest are computed locally from `bytes`.
fn pin_for(bytes: &[u8], version: &'static str) -> &'static AntigravityPin {
    let digest = leak_str(hex::encode(sha2::Sha256::digest(bytes)));
    make_pin(version, bytes.len() as u64, digest)
}

struct FixtureSource {
    bytes: Vec<u8>,
    calls: Arc<AtomicUsize>,
    abort_after_bytes: Option<usize>,
}

impl FixtureSource {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            calls: Arc::new(AtomicUsize::new(0)),
            abort_after_bytes: None,
        }
    }

    fn interrupted(bytes: Vec<u8>, after: usize) -> Self {
        Self {
            bytes,
            calls: Arc::new(AtomicUsize::new(0)),
            abort_after_bytes: Some(after),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ArchiveSource for FixtureSource {
    async fn stream_to(
        &self,
        _pin: &AntigravityPin,
        sink: &mut ArchiveSink,
    ) -> Result<(), AntigravityInstallError> {
        const CHUNK: usize = 8 * 1024;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut offset = 0_usize;
        while offset < self.bytes.len() {
            if self.abort_after_bytes.is_some_and(|limit| offset >= limit) {
                return Err(AntigravityInstallError::Http {
                    message: "fixture transfer interrupted".into(),
                });
            }
            let end = (offset + CHUNK).min(self.bytes.len());
            sink.write_chunk(&self.bytes[offset..end])?;
            offset = end;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Filesystem assertions
// ---------------------------------------------------------------------------

fn version_directory(root: &Path, version: &str) -> PathBuf {
    root.join(VERSIONS_DIRECTORY).join(version)
}

/// Every name under `versions/`, including any staging or retired leftover, so
/// a residue assertion cannot pass by looking only at published names.
fn version_entries(root: &Path) -> Vec<String> {
    directory_entries(&root.join(VERSIONS_DIRECTORY))
}

fn download_entries(root: &Path) -> Vec<String> {
    directory_entries(&root.join(DOWNLOADS_DIRECTORY))
}

fn directory_entries(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn assert_owner_only(path: &Path) {
    let metadata = std::fs::metadata(path).expect("stat installed path");
    let mode = haider_platform::metadata_mode(&metadata) & 0o777;
    assert_eq!(
        mode,
        0o700,
        "{} should be owner-only, was {mode:o}",
        path.display()
    );
}

async fn install_fixture(
    root: &Path,
    archive: Vec<u8>,
    version: &'static str,
) -> (&'static AntigravityPin, FixtureSource) {
    let pin = pin_for(&archive, version);
    let source = FixtureSource::new(archive);
    let installer = AntigravityInstaller::new(root);
    let outcome = installer
        .ensure_installed(pin, &source)
        .await
        .expect("fixture install");
    assert!(matches!(outcome, InstallOutcome::Installed(_)));
    (pin, source)
}

// ---------------------------------------------------------------------------
// 1. Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn installs_pinned_archive_with_owner_only_modes_and_activates_version() {
    let root = tempfile::tempdir().expect("temp root");
    let archive = valid_archive();
    let pin = pin_for(&archive, "1.1.1-fixture");
    let source = FixtureSource::new(archive);
    let installer = AntigravityInstaller::new(root.path());

    let outcome = installer
        .ensure_installed(pin, &source)
        .await
        .expect("install the fixture archive");
    let InstallOutcome::Installed(installation) = &outcome else {
        panic!("expected a fresh install, got {outcome:?}");
    };

    let expected_directory = version_directory(root.path(), "1.1.1-fixture");
    assert_eq!(installation.version(), "1.1.1-fixture");
    assert_eq!(installation.directory(), expected_directory);
    assert_eq!(installation.executable(), expected_directory.join(EXE));
    assert_eq!(installation.helper(), expected_directory.join(HELPER));
    assert_eq!(installation.args().to_vec(), vec!["--uid=".to_owned()]);

    assert_eq!(
        std::fs::read(installation.executable()).expect("read executable"),
        EXE_BODY
    );
    assert_eq!(
        std::fs::read(installation.helper()).expect("read helper"),
        HELPER_BODY
    );

    assert_eq!(
        installer.active_version().expect("read pointer").as_deref(),
        Some("1.1.1-fixture")
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join(ACTIVE_POINTER)).expect("read pointer file"),
        "1.1.1-fixture\n"
    );

    assert_owner_only(installation.directory());
    assert_owner_only(installation.executable());
    assert_owner_only(installation.helper());
    assert_owner_only(&root.path().join(VERSIONS_DIRECTORY));

    assert_eq!(source.calls(), 1);
    assert_eq!(
        version_entries(root.path()),
        vec!["1.1.1-fixture".to_owned()]
    );
    // The verified archive is removed once its tree exists.
    assert!(download_entries(root.path()).is_empty());
}

// ---------------------------------------------------------------------------
// 2. Digest mismatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_mismatch_refuses_and_installs_nothing() {
    let root = tempfile::tempdir().expect("temp root");
    let archive = valid_archive();
    let pin = make_pin("1.1.1-fixture", archive.len() as u64, WRONG_DIGEST);
    let source = FixtureSource::new(archive);
    let installer = AntigravityInstaller::new(root.path());

    let error = installer
        .ensure_installed(pin, &source)
        .await
        .expect_err("a wrong digest must refuse");
    match error {
        AntigravityInstallError::ArchiveDigestMismatch { expected, actual } => {
            assert_eq!(expected, WRONG_DIGEST);
            assert_ne!(actual, WRONG_DIGEST);
        }
        other => panic!("expected a digest mismatch, got {other:?}"),
    }

    assert_eq!(installer.active_version().expect("read pointer"), None);
    assert!(version_entries(root.path()).is_empty());
    assert!(download_entries(root.path()).is_empty());
}

// ---------------------------------------------------------------------------
// 3. Size mismatch, both directions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn short_archive_and_overlong_archive_are_both_refused() {
    let archive = valid_archive();
    let digest = leak_str(hex::encode(sha2::Sha256::digest(&archive)));

    // Short: the body ends before the pinned size.
    let short_root = tempfile::tempdir().expect("temp root");
    let short_pin = make_pin("1.1.1-fixture", archive.len() as u64 + 1, digest);
    let short_source = FixtureSource::new(archive.clone());
    let short_installer = AntigravityInstaller::new(short_root.path());
    let error = short_installer
        .ensure_installed(short_pin, &short_source)
        .await
        .expect_err("a short archive must refuse");
    match error {
        AntigravityInstallError::ArchiveSizeMismatch { expected, actual } => {
            assert_eq!(expected, archive.len() as u64 + 1);
            assert_eq!(actual, archive.len() as u64);
        }
        other => panic!("expected a size mismatch, got {other:?}"),
    }
    assert!(version_entries(short_root.path()).is_empty());
    assert!(download_entries(short_root.path()).is_empty());

    // Long: the stream is abandoned the moment it passes the pinned size,
    // rather than being buffered and measured afterwards.
    let long_root = tempfile::tempdir().expect("temp root");
    let pinned = archive.len() as u64 - 1;
    let long_pin = make_pin("1.1.1-fixture", pinned, digest);
    let long_source = FixtureSource::new(archive);
    let long_installer = AntigravityInstaller::new(long_root.path());
    let error = long_installer
        .ensure_installed(long_pin, &long_source)
        .await
        .expect_err("an overlong archive must refuse");
    match error {
        AntigravityInstallError::ArchiveOverran { pinned_bytes } => {
            assert_eq!(pinned_bytes, pinned);
        }
        other => panic!("expected an overrun, got {other:?}"),
    }
    assert!(version_entries(long_root.path()).is_empty());
    assert!(download_entries(long_root.path()).is_empty());
}

// ---------------------------------------------------------------------------
// 4-8, 18, 20. Hostile archive entries
// ---------------------------------------------------------------------------

async fn refuse_archive(archive: Vec<u8>) -> AntigravityInstallError {
    let root = tempfile::tempdir().expect("temp root");
    let pin = pin_for(&archive, "1.1.1-fixture");
    let source = FixtureSource::new(archive);
    let installer = AntigravityInstaller::new(root.path());
    let error = installer
        .ensure_installed(pin, &source)
        .await
        .expect_err("a hostile archive must refuse");
    // Whatever the fault, nothing is published and nothing is left staged.
    assert_eq!(installer.active_version().expect("read pointer"), None);
    assert!(
        version_entries(root.path()).is_empty(),
        "a refused archive left {:?} behind",
        version_entries(root.path())
    );
    assert!(download_entries(root.path()).is_empty());
    error
}

#[tokio::test]
async fn parent_directory_entry_is_refused() {
    let archive = build_archive(vec![
        Entry::Stored {
            name: "../evil",
            data: b"escape".to_vec(),
        },
        Entry::Stored {
            name: EXE,
            data: EXE_BODY.to_vec(),
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ]);
    match refuse_archive(archive).await {
        AntigravityInstallError::UnsafeEntryPath { entry, fault } => {
            assert_eq!(entry, "../evil");
            assert_eq!(fault, EntryPathFault::ParentComponent);
        }
        other => panic!("expected a traversal refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn absolute_path_entry_is_refused() {
    let archive = build_archive(vec![
        Entry::Stored {
            name: "/evil",
            data: b"escape".to_vec(),
        },
        Entry::Stored {
            name: EXE,
            data: EXE_BODY.to_vec(),
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ]);
    match refuse_archive(archive).await {
        AntigravityInstallError::UnsafeEntryPath { entry, fault } => {
            assert_eq!(entry, "/evil");
            assert_eq!(fault, EntryPathFault::Absolute);
        }
        other => panic!("expected an absolute-path refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn drive_letter_entry_is_refused() {
    let archive = build_archive(vec![
        Entry::Stored {
            name: "C:evil",
            data: b"escape".to_vec(),
        },
        Entry::Stored {
            name: EXE,
            data: EXE_BODY.to_vec(),
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ]);
    match refuse_archive(archive).await {
        AntigravityInstallError::UnsafeEntryPath { entry, fault } => {
            assert_eq!(entry, "C:evil");
            assert_eq!(fault, EntryPathFault::DriveOrUnc);
        }
        other => panic!("expected a drive-letter refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn symlink_entry_is_refused() {
    let archive = build_archive(vec![
        Entry::Symlink {
            name: EXE,
            target: "/etc/hosts",
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ]);
    match refuse_archive(archive).await {
        AntigravityInstallError::NonRegularEntry { entry, fault } => {
            assert_eq!(entry, EXE);
            assert_eq!(fault, EntryKindFault::Symlink);
        }
        other => panic!("expected a symlink refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn directory_entry_is_refused() {
    let archive = build_archive(vec![
        Entry::Directory { name: "nested" },
        Entry::Stored {
            name: EXE,
            data: EXE_BODY.to_vec(),
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ]);
    match refuse_archive(archive).await {
        AntigravityInstallError::NonRegularEntry { entry, fault } => {
            assert_eq!(entry, "nested/");
            assert_eq!(fault, EntryKindFault::Directory);
        }
        other => panic!("expected a directory refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn unexpected_extra_entry_is_refused() {
    let archive = build_archive(vec![
        Entry::Stored {
            name: EXE,
            data: EXE_BODY.to_vec(),
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
        Entry::Stored {
            name: "README.txt",
            data: b"extra".to_vec(),
        },
    ]);
    match refuse_archive(archive).await {
        AntigravityInstallError::UnexpectedEntry { entry } => {
            assert_eq!(entry, "README.txt");
        }
        other => panic!("expected an unexpected-entry refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_entry_name_is_refused() {
    match refuse_archive(duplicate_name_archive()).await {
        AntigravityInstallError::DuplicateEntry { declared, distinct } => {
            assert_eq!(declared, 2);
            assert_eq!(distinct, 1);
        }
        other => panic!("expected a duplicate-name refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_entry_count_is_refused_before_any_entry_is_opened() {
    let mut entries = vec![
        Entry::Stored {
            name: EXE,
            data: EXE_BODY.to_vec(),
        },
        Entry::Stored {
            name: HELPER,
            data: HELPER_BODY.to_vec(),
        },
    ];
    let over = MAX_ARCHIVE_ENTRIES + 1;
    let filler: Vec<String> = (0..over - 2)
        .map(|index| format!("filler-{index}"))
        .collect();
    for name in &filler {
        entries.push(Entry::Stored {
            name,
            data: b"filler".to_vec(),
        });
    }
    match refuse_archive(build_archive(entries)).await {
        AntigravityInstallError::EntryCountExceeded { declared, limit } => {
            assert_eq!(declared, over);
            assert_eq!(limit, MAX_ARCHIVE_ENTRIES);
        }
        other => panic!("expected an entry-count refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 9. Zip bomb
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compression_bomb_aborts_before_writing_its_declared_size() {
    const DECLARED: u64 = 16 * 1024 * 1024;
    match refuse_archive(compression_bomb_archive()).await {
        AntigravityInstallError::CompressionRatioExceeded {
            entry,
            written_bytes,
            allowance_bytes,
        } => {
            assert_eq!(entry, EXE);
            // The abort is incremental: it happens once the entry passes its
            // allowance, plus at most one 64 KiB read, never after
            // materializing the declared 16 MiB.
            assert!(
                written_bytes > allowance_bytes,
                "the guard tripped below its own allowance: {written_bytes} <= {allowance_bytes}"
            );
            assert!(
                written_bytes <= allowance_bytes + 64 * 1024,
                "the guard overshot a read chunk: {written_bytes} > {allowance_bytes} + 64 KiB"
            );
            assert!(
                written_bytes * 8 < DECLARED,
                "the bomb was nearly fully written before the abort: {written_bytes}"
            );
        }
        other => panic!("expected a compression-ratio refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 10. Missing helper
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_helper_binary_refuses_activation() {
    let archive = build_archive(vec![Entry::Stored {
        name: EXE,
        data: EXE_BODY.to_vec(),
    }]);
    match refuse_archive(archive).await {
        AntigravityInstallError::MissingEntry { entry } => {
            assert_eq!(entry, HELPER);
        }
        other => panic!("expected a missing-helper refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 11. Unsafe installed permissions
// ---------------------------------------------------------------------------

/// A wrong-owner binary is refused by the same `metadata_is_current_user`
/// branch of `inspect_executable` that this test drives through the mode
/// branch; ownership cannot be forged without root, so it is covered by
/// inspection rather than by a chown here.
#[cfg(unix)]
#[tokio::test]
async fn world_writable_installed_binary_refuses_activation() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("temp root");
    let (pin, source) = install_fixture(root.path(), valid_archive(), "1.1.1-fixture").await;
    let installer = AntigravityInstaller::new(root.path());
    let executable = version_directory(root.path(), "1.1.1-fixture").join(EXE);

    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o707))
        .expect("widen the installed binary");

    let error = installer
        .resolve(pin)
        .expect_err("a world-writable binary must refuse");
    match error {
        AntigravityInstallError::InsecurePermissions { path, mode } => {
            assert_eq!(path, executable);
            assert_eq!(mode & 0o777, 0o707);
        }
        other => panic!("expected an insecure-permission refusal, got {other:?}"),
    }

    // Fail closed: the suspicious tree is reported, never quietly replaced.
    let error = installer
        .ensure_installed(pin, &source)
        .await
        .expect_err("install must not paper over a suspicious tree");
    assert!(matches!(
        error,
        AntigravityInstallError::InsecurePermissions { .. }
    ));
    assert_eq!(source.calls(), 1, "no second transfer may be attempted");
    assert!(executable.exists());
}

// ---------------------------------------------------------------------------
// 12. Atomic activation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interrupted_install_never_moves_the_active_pointer() {
    let root = tempfile::tempdir().expect("temp root");
    install_fixture(root.path(), valid_archive(), "1.0.0-fixture").await;
    let installer = AntigravityInstaller::new(root.path());
    let baseline = std::fs::read(root.path().join(ACTIVE_POINTER)).expect("read pointer");

    // A transfer that dies part way through.
    let large = large_valid_archive();
    let next_pin = pin_for(&large, "2.0.0-fixture");
    let interrupted = FixtureSource::interrupted(large, 64 * 1024);
    let error = installer
        .ensure_installed(next_pin, &interrupted)
        .await
        .expect_err("an interrupted transfer must refuse");
    assert!(matches!(error, AntigravityInstallError::Http { .. }));
    assert_eq!(
        installer.active_version().expect("read pointer").as_deref(),
        Some("1.0.0-fixture")
    );
    assert_eq!(
        version_entries(root.path()),
        vec!["1.0.0-fixture".to_owned()]
    );
    assert!(download_entries(root.path()).is_empty());

    // A transfer that completes but whose archive is refused during
    // extraction: the failure now happens AFTER bytes hit the disk.
    let bomb = compression_bomb_archive();
    let bomb_pin = pin_for(&bomb, "2.0.0-fixture");
    let bomb_source = FixtureSource::new(bomb);
    let error = installer
        .ensure_installed(bomb_pin, &bomb_source)
        .await
        .expect_err("a refused extraction must not activate");
    assert!(matches!(
        error,
        AntigravityInstallError::CompressionRatioExceeded { .. }
    ));
    assert_eq!(
        installer.active_version().expect("read pointer").as_deref(),
        Some("1.0.0-fixture")
    );
    assert_eq!(
        std::fs::read(root.path().join(ACTIVE_POINTER)).expect("read pointer"),
        baseline
    );
    // No staging or retired residue is left under `versions/`.
    assert_eq!(
        version_entries(root.path()),
        vec!["1.0.0-fixture".to_owned()]
    );
    assert!(download_entries(root.path()).is_empty());

    // The surviving tree is still usable.
    let previous = pin_for(&valid_archive(), "1.0.0-fixture");
    assert!(installer.resolve(previous).expect("resolve").is_some());
}

// ---------------------------------------------------------------------------
// 13, 17. Leases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn leased_version_survives_a_later_install_until_the_lease_is_released() {
    let root = tempfile::tempdir().expect("temp root");
    let (pin, source) = install_fixture(root.path(), valid_archive(), "1.1.1-fixture").await;
    let installer = AntigravityInstaller::new(root.path());
    let directory = version_directory(root.path(), "1.1.1-fixture");
    let executable = directory.join(EXE);
    let helper = directory.join(HELPER);

    let lease = installer
        .acquire_lease("1.1.1-fixture")
        .expect("acquire a lease");
    assert!(installer.is_version_leased("1.1.1-fixture").expect("probe"));

    // Damage the tree so a reinstall would otherwise be required, then ask for
    // one: the live lease refuses it, before any transfer is attempted.
    std::fs::remove_file(&helper).expect("remove the helper");
    assert!(installer.resolve(pin).expect("resolve").is_none());
    let error = installer
        .ensure_installed(pin, &source)
        .await
        .expect_err("a leased version must not be replaced");
    match error {
        AntigravityInstallError::VersionLeased { version } => {
            assert_eq!(version, "1.1.1-fixture");
        }
        other => panic!("expected a lease refusal, got {other:?}"),
    }
    assert_eq!(source.calls(), 1, "the refusal must precede any transfer");
    assert!(
        executable.exists(),
        "a leased version's executable must never be removed"
    );

    lease.release().expect("release the lease");
    assert!(!installer.is_version_leased("1.1.1-fixture").expect("probe"));

    let outcome = installer
        .ensure_installed(pin, &source)
        .await
        .expect("install once the lease is gone");
    assert!(matches!(outcome, InstallOutcome::Installed(_)));
    assert_eq!(source.calls(), 2);
    assert!(helper.exists());
    assert_eq!(
        version_entries(root.path()),
        vec!["1.1.1-fixture".to_owned()]
    );
}

#[test]
fn stale_lease_left_by_a_dead_holder_is_reclaimed() {
    let root = tempfile::tempdir().expect("temp root");
    let installer = AntigravityInstaller::new(root.path());

    // A holder that exited without releasing: the file survives, the kernel
    // dropped the lock.
    let lease = installer.acquire_lease("1.1.1-fixture").expect("acquire");
    let path = lease.path().to_path_buf();
    assert!(installer.is_version_leased("1.1.1-fixture").expect("probe"));
    drop(lease);
    assert!(path.exists(), "a crashed holder leaves its lease file");
    assert!(
        !installer.is_version_leased("1.1.1-fixture").expect("probe"),
        "an unlocked lease file must not pin a version"
    );
    assert!(!path.exists(), "the sweep removes the reclaimed lease");

    // A lease file nobody ever locked — the hard-kill shape — is reclaimed the
    // same way.
    let orphan = root
        .path()
        .join(LEASES_DIRECTORY)
        .join("1.1.1-fixture")
        .join("deadbeefdeadbeefdeadbeefdeadbeef.lease");
    std::fs::write(&orphan, b"pid=1\n").expect("forge an orphan lease");
    assert!(!installer.is_version_leased("1.1.1-fixture").expect("probe"));
    assert!(!orphan.exists());

    // Two live leases on one version are both honoured.
    let first = installer.acquire_lease("1.1.1-fixture").expect("acquire");
    let second = installer.acquire_lease("1.1.1-fixture").expect("acquire");
    assert_ne!(first.path(), second.path());
    first.release().expect("release the first");
    assert!(
        installer.is_version_leased("1.1.1-fixture").expect("probe"),
        "the second lease still holds the version"
    );
    second.release().expect("release the second");
    assert!(!installer.is_version_leased("1.1.1-fixture").expect("probe"));
}

// ---------------------------------------------------------------------------
// 14. Unsupported platform
// ---------------------------------------------------------------------------

#[test]
fn intel_macos_is_unsupported_and_never_falls_back() {
    let error = pin_for_platform("macos", "x86_64")
        .expect_err("Google publishes no Intel macOS Antigravity build");
    match error {
        AntigravityInstallError::UnsupportedPlatform { os, arch } => {
            assert_eq!(os, "macos");
            assert_eq!(arch, "x86_64");
        }
        other => panic!("expected an unsupported-platform error, got {other:?}"),
    }
    assert!(
        ANTIGRAVITY_PINS
            .iter()
            .all(|pin| pin.platform_key() != "darwin-x86_64"),
        "there must be no Intel macOS entry to fall back to"
    );
    for (os, arch) in [
        ("freebsd", "x86_64"),
        ("macos", "arm"),
        ("linux", "riscv64"),
        ("windows", "x86"),
    ] {
        assert!(
            pin_for_platform(os, arch).is_err(),
            "{os}-{arch} must not resolve to another platform's archive"
        );
    }
    // The host resolver is the platform resolver; it invents nothing.
    assert_eq!(
        pin_for_host().ok(),
        pin_for_platform(std::env::consts::OS, std::env::consts::ARCH).ok()
    );
}

// ---------------------------------------------------------------------------
// 15. No auto-upgrade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn second_ensure_installed_does_no_transfer_and_does_not_move_the_pointer() {
    let root = tempfile::tempdir().expect("temp root");
    let archive = valid_archive();
    let pin = pin_for(&archive, "1.1.1-fixture");
    let source = FixtureSource::new(archive);
    let installer = AntigravityInstaller::new(root.path());

    installer
        .ensure_installed(pin, &source)
        .await
        .expect("first install");
    let pointer = root.path().join(ACTIVE_POINTER);
    let bytes = std::fs::read(&pointer).expect("read pointer");
    let modified = std::fs::metadata(&pointer)
        .expect("stat pointer")
        .modified()
        .expect("pointer mtime");

    let outcome = installer
        .ensure_installed(pin, &source)
        .await
        .expect("second call");
    let InstallOutcome::AlreadyPresent(installation) = &outcome else {
        panic!("a valid install must not be redone, got {outcome:?}");
    };
    assert_eq!(installation.version(), "1.1.1-fixture");
    assert_eq!(source.calls(), 1, "no second transfer may happen");
    assert_eq!(std::fs::read(&pointer).expect("read pointer"), bytes);
    assert_eq!(
        std::fs::metadata(&pointer)
            .expect("stat pointer")
            .modified()
            .expect("pointer mtime"),
        modified,
        "the active pointer must not be rewritten"
    );
    assert_eq!(
        version_entries(root.path()),
        vec!["1.1.1-fixture".to_owned()]
    );
}

// ---------------------------------------------------------------------------
// 16. The pin table itself
// ---------------------------------------------------------------------------

struct ExpectedPin {
    platform_key: &'static str,
    archive_url: &'static str,
    archive_size_bytes: u64,
    archive_sha256: &'static str,
    executable_name: &'static str,
    helper_name: &'static str,
    extra_args: &'static [&'static str],
}

/// Transcribed independently from `docs/testing/v0.0.970/_antigravity-pins.md`
/// so a slipped digit in the Rust table fails here rather than at install time.
const RELEASE_TABLE: &[ExpectedPin] = &[
    ExpectedPin {
        platform_key: "darwin-aarch64",
        archive_url: "https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-agy_acp_server_1.1.1-darwin-arm64.zip",
        archive_size_bytes: 316_014_828,
        archive_sha256: "fdfa915652cdb7ba8085cc8fffed072cbe009251aa2c951aabdda07a8c28a189",
        executable_name: "agy_acp_server.par",
        helper_name: "localharness_external",
        extra_args: &[],
    },
    ExpectedPin {
        platform_key: "linux-x86_64",
        archive_url: "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-x86_64.zip",
        archive_size_bytes: 681_969_407,
        archive_sha256: "38f62d01b32deb0907b3d39a71ec301fd36369f6ffd1cf262d4af385177f79df",
        executable_name: "agy_acp_server.par",
        helper_name: "localharness_external",
        extra_args: &["--uid="],
    },
    ExpectedPin {
        platform_key: "linux-aarch64",
        archive_url: "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-arm64.zip",
        archive_size_bytes: 656_572_786,
        archive_sha256: "ed69e64b308fcb123ab54bf3277bf9cb0d651064f885ea5aab0ff520c7175398",
        executable_name: "agy_acp_server.par",
        helper_name: "localharness_external",
        extra_args: &["--uid="],
    },
    ExpectedPin {
        platform_key: "windows-x86_64",
        archive_url: "https://dl.google.com/agy-extensions/releases/windows/agy-acp-server-agy_acp_server_1.1.1-windows-x86_64.zip",
        archive_size_bytes: 468_238_392,
        archive_sha256: "47cb50eef14f0a4655d78cfcfda869bcea7aaee5f9787e936bc2935ea612c3b8",
        executable_name: "agy_acp_server.exe",
        helper_name: "localharness_external.exe",
        extra_args: &[],
    },
    ExpectedPin {
        platform_key: "windows-aarch64",
        archive_url: "https://dl.google.com/agy-extensions/releases/windows/agy-acp-server-agy_acp_server_1.1.1-windows-arm64.zip",
        archive_size_bytes: 468_521_191,
        archive_sha256: "35f4b1f47ba6a3fea7b0a3e30010df5ea73a64b4f0e7cf991cddc673ddfbcafc",
        executable_name: "agy_acp_server.exe",
        helper_name: "localharness_external.exe",
        extra_args: &[],
    },
];

#[test]
fn pin_table_is_release_owned_and_covers_exactly_the_five_supported_platforms() {
    assert_eq!(ANTIGRAVITY_PINS.len(), 5);
    assert_eq!(ANTIGRAVITY_VERSION, "1.1.1");

    let keys: Vec<&str> = ANTIGRAVITY_PINS
        .iter()
        .map(AntigravityPin::platform_key)
        .collect();
    assert_eq!(
        keys,
        vec![
            "darwin-aarch64",
            "linux-x86_64",
            "linux-aarch64",
            "windows-x86_64",
            "windows-aarch64",
        ]
    );
    assert!(!keys.contains(&"darwin-x86_64"));

    let mut digests: Vec<&str> = ANTIGRAVITY_PINS
        .iter()
        .map(AntigravityPin::archive_sha256)
        .collect();
    digests.sort_unstable();
    digests.dedup();
    assert_eq!(digests.len(), 5, "every platform must have its own digest");

    for pin in ANTIGRAVITY_PINS {
        let key = pin.platform_key();
        assert_eq!(pin.version(), ANTIGRAVITY_VERSION, "{key}");

        assert_eq!(pin.archive_sha256().len(), 64, "{key} digest length");
        assert!(
            pin.archive_sha256()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
            "{key} digest must be lowercase hex"
        );
        assert!(pin.archive_size_bytes() > 0, "{key} size");

        assert!(
            pin.archive_url().starts_with("https://dl.google.com/"),
            "{key} url"
        );
        let url = Url::parse(pin.archive_url()).expect("pinned url parses");
        assert!(approved_archive_origin(&url), "{key} origin");

        let windows = key.starts_with("windows-");
        assert_eq!(
            pin.executable_name().ends_with(".exe"),
            windows,
            "{key} exe"
        );
        assert_eq!(pin.helper_name().ends_with(".exe"), windows, "{key} helper");
        assert_ne!(pin.executable_name(), pin.helper_name(), "{key} names");

        // Only Linux takes the extra `--uid=` argv, with an empty value.
        let expected_args: &[&str] = if key.starts_with("linux-") {
            &["--uid="]
        } else {
            &[]
        };
        assert_eq!(pin.extra_args(), expected_args, "{key} argv");
    }
}

#[test]
fn pin_table_matches_the_release_measured_digests_and_sizes() {
    assert_eq!(ANTIGRAVITY_PINS.len(), RELEASE_TABLE.len());
    for (pin, expected) in ANTIGRAVITY_PINS.iter().zip(RELEASE_TABLE) {
        let key = expected.platform_key;
        assert_eq!(pin.platform_key(), key);
        assert_eq!(pin.archive_url(), expected.archive_url, "{key} url");
        assert_eq!(
            pin.archive_size_bytes(),
            expected.archive_size_bytes,
            "{key} size"
        );
        assert_eq!(
            pin.archive_sha256(),
            expected.archive_sha256,
            "{key} digest"
        );
        assert_eq!(pin.executable_name(), expected.executable_name, "{key} exe");
        assert_eq!(pin.helper_name(), expected.helper_name, "{key} helper");
        assert_eq!(pin.extra_args(), expected.extra_args, "{key} argv");
    }
}

// ---------------------------------------------------------------------------
// Origin policy
// ---------------------------------------------------------------------------

#[test]
fn only_the_google_download_origin_over_tls_is_approved() {
    for approved in [
        "https://dl.google.com/agy-extensions/releases/macos/x.zip",
        "https://dl.google.com/",
    ] {
        let url = Url::parse(approved).expect("parse url");
        assert!(approved_archive_origin(&url), "{approved}");
    }
    for refused in [
        // Plaintext, even on the right host.
        "http://dl.google.com/x.zip",
        // A suffix that merely looks like the approved host.
        "https://dl.google.com.evil.example/x.zip",
        // A subdomain of the approved host.
        "https://cdn.dl.google.com/x.zip",
        // Another Google origin is still not the approved one.
        "https://storage.googleapis.com/x.zip",
        "https://evil.example/x.zip",
        "file:///etc/hosts",
    ] {
        let url = Url::parse(refused).expect("parse url");
        assert!(!approved_archive_origin(&url), "{refused}");
    }
}

// ---------------------------------------------------------------------------
// Verification can never be skipped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_malformed_pin_digest_refuses_before_any_transfer() {
    let root = tempfile::tempdir().expect("temp root");
    let archive = valid_archive();
    let pin = make_pin("1.1.1-fixture", archive.len() as u64, "not-a-digest");
    let source = FixtureSource::new(archive);
    let installer = AntigravityInstaller::new(root.path());

    let error = installer
        .ensure_installed(pin, &source)
        .await
        .expect_err("a malformed pin digest must refuse");
    assert!(matches!(
        error,
        AntigravityInstallError::MalformedPinDigest { .. }
    ));
    assert_eq!(source.calls(), 0, "nothing may be fetched unverifiably");
    assert!(version_entries(root.path()).is_empty());
}

#[tokio::test]
async fn an_unusable_version_string_never_reaches_the_filesystem() {
    let root = tempfile::tempdir().expect("temp root");
    let archive = valid_archive();
    let source = FixtureSource::new(archive.clone());
    let installer = AntigravityInstaller::new(root.path());
    for version in ["../escape", "", ".", "..", "a/b", ".hidden"] {
        let pin = pin_for(&archive, leak_str(version.to_owned()));
        let error = installer
            .ensure_installed(pin, &source)
            .await
            .expect_err("an unusable version must refuse");
        assert!(
            matches!(error, AntigravityInstallError::InvalidVersion { .. }),
            "version {version:?} produced {error:?}"
        );
    }
    assert_eq!(source.calls(), 0);
    assert!(version_entries(root.path()).is_empty());
}
