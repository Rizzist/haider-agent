#![allow(clippy::unwrap_used)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use zeroize::Zeroizing;

use crate::{CredentialAlias, EncryptedFileVault, ErrorCode, Vault};

fn fixture() -> (tempfile::TempDir, EncryptedFileVault, CredentialAlias) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("vault");
    let vault = EncryptedFileVault::new(root, Zeroizing::new([7; 32])).unwrap();
    (
        dir,
        vault,
        CredentialAlias::new("0123456789abcdef::synthetic"),
    )
}

fn path(dir: &tempfile::TempDir, alias: &CredentialAlias) -> std::path::PathBuf {
    dir.path()
        .join("vault")
        .join(format!("{}.vault", hex::encode(alias.as_str())))
}

#[test]
fn hav1_roundtrip_empty_missing_delete_and_refresh_lock() {
    let (dir, vault, alias) = fixture();
    assert!(vault.list().unwrap().is_empty());
    assert_eq!(
        vault.resolve(&alias).unwrap_err().code,
        ErrorCode::CredentialMissing
    );
    vault.put(&alias, b"").unwrap();
    assert_eq!(std::fs::read(path(&dir, &alias)).unwrap().len(), 32);
    assert!(vault.resolve(&alias).unwrap().expose_secret().is_empty());
    vault.put(&alias, b"synthetic-test-secret").unwrap();
    assert_eq!(
        vault.resolve(&alias).unwrap().expose_secret(),
        b"synthetic-test-secret"
    );
    assert_eq!(vault.list().unwrap(), vec![alias.clone()]);
    let first = vault.try_refresh_lock(&alias).unwrap().unwrap();
    let second = EncryptedFileVault::new(
        dir.path().canonicalize().unwrap().join("vault"),
        Zeroizing::new([7; 32]),
    )
    .unwrap();
    assert!(second.try_refresh_lock(&alias).unwrap().is_none());
    drop(first);
    assert!(second.try_refresh_lock(&alias).unwrap().is_some());
    vault.delete(&alias).unwrap();
    vault.delete(&alias).unwrap();
    assert!(vault.list().unwrap().is_empty());
}

#[test]
fn hav1_authenticates_key_nonce_tag_magic_and_physical_alias() {
    let (dir, vault, alias) = fixture();
    vault.put(&alias, b"synthetic-secret-marker-971").unwrap();
    let entry = path(&dir, &alias);
    let record = std::fs::read(&entry).unwrap();
    assert_eq!(&record[..4], b"HAV1");
    assert_eq!(record.len(), 32 + b"synthetic-secret-marker-971".len());
    let wrong = EncryptedFileVault::new(
        dir.path().canonicalize().unwrap().join("vault"),
        Zeroizing::new([8; 32]),
    )
    .unwrap();
    assert_eq!(
        wrong.resolve(&alias).unwrap_err().code,
        ErrorCode::StoreCorrupt
    );
    assert_eq!(
        wrong.authenticate_existing().unwrap_err().code,
        ErrorCode::StoreCorrupt
    );
    assert!(wrong.put(&alias, b"replacement").is_err());
    assert!(wrong.delete(&alias).is_err());
    assert_eq!(std::fs::read(&entry).unwrap(), record);
    for offset in [0, 4, 15, 16, record.len() - 1] {
        let mut damaged = record.clone();
        damaged[offset] ^= 1;
        std::fs::write(&entry, &damaged).unwrap();
        assert_eq!(
            vault.resolve(&alias).unwrap_err().code,
            ErrorCode::StoreCorrupt
        );
    }
    std::fs::write(&entry, &record).unwrap();
    let other = CredentialAlias::new("fedcba9876543210::synthetic");
    std::fs::rename(&entry, path(&dir, &other)).unwrap();
    assert_eq!(
        vault.resolve(&other).unwrap_err().code,
        ErrorCode::StoreCorrupt
    );
}

#[test]
fn hav1_bounds_plaintext_and_encoded_records_before_release() {
    let (dir, vault, alias) = fixture();
    vault.put(&alias, &vec![42; 524_288]).unwrap();
    assert_eq!(
        vault.resolve(&alias).unwrap().expose_secret().len(),
        524_288
    );
    assert_eq!(
        vault.put(&alias, &vec![42; 524_289]).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    let entry = path(&dir, &alias);
    for length in [0, 3, 15, 31, 524_321] {
        std::fs::write(&entry, vec![0; length]).unwrap();
        assert_eq!(
            vault.resolve(&alias).unwrap_err().code,
            ErrorCode::StoreCorrupt
        );
    }
}

#[test]
fn hav1_atomic_replace_has_private_ciphertext_and_fresh_nonces() {
    let (dir, vault, alias) = fixture();
    let marker = b"synthetic-no-plaintext-marker-971";
    vault.put(&alias, marker).unwrap();
    let entry = path(&dir, &alias);
    let before = std::fs::read(&entry).unwrap();
    let inode = std::fs::metadata(&entry).unwrap().ino();
    vault.put(&alias, marker).unwrap();
    let after = std::fs::read(&entry).unwrap();
    assert_ne!(before[4..16], after[4..16]);
    assert_ne!(inode, std::fs::metadata(&entry).unwrap().ino());
    assert_eq!(
        std::fs::metadata(&entry).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for file in std::fs::read_dir(dir.path().join("vault")).unwrap() {
        let file = file.unwrap();
        let bytes = std::fs::read(file.path()).unwrap();
        assert!(!bytes.windows(marker.len()).any(|bytes| bytes == marker));
        assert!(!file.file_name().to_string_lossy().ends_with(".tmp"));
    }
    let vault = Arc::new(vault);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..50 {
                vault.put(&alias, &[i; 1024]).unwrap();
            }
        });
        for _ in 0..100 {
            let secret = vault.resolve(&alias).unwrap();
            assert!(
                secret.expose_secret() == marker
                    || (secret.expose_secret().len() == 1024
                        && secret
                            .expose_secret()
                            .iter()
                            .all(|b| *b == secret.expose_secret()[0]))
            );
        }
    });
}

#[test]
fn hav1_concurrent_replacements_preserve_complete_authenticated_records() {
    const WRITERS: usize = 4;
    const READERS: usize = 4;
    let (dir, vault, alias) = fixture();
    let records: Vec<Vec<u8>> = (0..WRITERS)
        .map(|writer| vec![writer as u8; 1024 + writer * 4096])
        .collect();
    vault.put(&alias, &records[0]).unwrap();
    // Independent vault instances must work without a process-local writer lock.
    let vaults: Vec<_> = (0..WRITERS + READERS)
        .map(|_| {
            EncryptedFileVault::new(
                dir.path().canonicalize().unwrap().join("vault"),
                Zeroizing::new([7; 32]),
            )
            .unwrap()
        })
        .collect();
    let start = Barrier::new(WRITERS + READERS);
    let writing = AtomicUsize::new(WRITERS);
    std::thread::scope(|scope| {
        for (writer, vault) in vaults[..WRITERS].iter().enumerate() {
            let (start, writing, alias, records) = (&start, &writing, &alias, &records);
            scope.spawn(move || {
                start.wait();
                let result = (0..250).try_for_each(|iteration| {
                    vault.put(alias, &records[(writer + iteration) % WRITERS])
                });
                // Readers must finish even when a writer reports a regression.
                writing.fetch_sub(1, Ordering::Release);
                result.unwrap();
            });
        }
        for vault in &vaults[WRITERS..] {
            let (start, writing, alias, records) = (&start, &writing, &alias, &records);
            scope.spawn(move || {
                start.wait();
                let mut reads = 0;
                while reads < 2000 || writing.load(Ordering::Acquire) != 0 {
                    let secret = vault.resolve(alias).unwrap();
                    assert!(
                        records
                            .iter()
                            .any(|record| record == secret.expose_secret())
                    );
                    reads += 1;
                }
            });
        }
    });
    assert_eq!(vault.list().unwrap(), vec![alias.clone()]);
    assert_eq!(
        std::fs::read_dir(dir.path().join("vault")).unwrap().count(),
        1
    );
    let secret = vault.resolve(&alias).unwrap();
    assert!(
        records
            .iter()
            .any(|record| record == secret.expose_secret())
    );
}

#[test]
fn hav1_reads_existing_release_record_without_format_migration() {
    let (dir, vault, alias) = fixture();
    // Produced by the unmodified writer at release candidate 0095d17a, using
    // this fixture's synthetic alias and [7; 32] key. Never regenerate via put.
    let record = hex::decode(concat!(
        "4841563151eb1b636e2ad8f2784721eb529a682e95edc766055fd11ca6ba301a1fb1",
        "6de689d3f0a343da6ba2b91b7a4a2ad8c8076f7109cf6713c99bb30a51c0334c16",
        "33bfc0ee57f79984"
    ))
    .unwrap();
    let entry = path(&dir, &alias);
    std::fs::write(&entry, &record).unwrap();
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o600)).unwrap();
    vault.authenticate_existing().unwrap();
    assert_eq!(
        vault.resolve(&alias).unwrap().expose_secret(),
        b"HAV1 release 0095d17a compatibility fixture"
    );
    assert_eq!(std::fs::read(&entry).unwrap(), record);
    vault.put(&alias, b"replacement").unwrap();
    let replaced = std::fs::read(&entry).unwrap();
    assert_eq!(&replaced[..4], b"HAV1");
    assert_eq!(replaced.len(), 32 + b"replacement".len());
    assert_eq!(
        vault.resolve(&alias).unwrap().expose_secret(),
        b"replacement"
    );
}

#[test]
fn hav1_refuses_symlinks_hardlinks_types_and_permissions() {
    let (dir, vault, alias) = fixture();
    vault.put(&alias, b"fixture").unwrap();
    let entry = path(&dir, &alias);
    let moved = dir.path().join("moved");
    std::fs::rename(&entry, &moved).unwrap();
    symlink(&moved, &entry).unwrap();
    assert_eq!(
        vault.resolve(&alias).unwrap_err().code,
        ErrorCode::StoreCorrupt
    );
    assert!(vault.list().is_err());
    assert!(vault.delete(&alias).is_err());
    std::fs::remove_file(&entry).unwrap();
    std::fs::hard_link(&moved, &entry).unwrap();
    assert!(vault.resolve(&alias).is_err());
    std::fs::remove_file(&entry).unwrap();
    std::fs::rename(&moved, &entry).unwrap();
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(vault.resolve(&alias).is_err());
    std::fs::remove_file(&entry).unwrap();
    std::fs::create_dir(&entry).unwrap();
    assert!(vault.resolve(&alias).is_err());
    let link = dir.path().join("link");
    symlink(dir.path().join("vault"), &link).unwrap();
    assert!(EncryptedFileVault::new(link, Zeroizing::new([7; 32])).is_err());
}
