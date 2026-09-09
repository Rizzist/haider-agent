//! HAV1 encrypted vault for Unix embedded hosts. All file operations are relative
//! to an owned directory handle; symlinks are never followed. Desktop defaults
//! continue to use FileVault.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use aes_gcm::{AeadInOut, Aes256Gcm, KeyInit};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use zeroize::Zeroizing;

use crate::{
    AccountsResult, CredentialAlias, ErrorCode, SecretHandle, Vault, VaultRefreshLock,
    accounts_error,
};

const MAGIC: &[u8; 4] = b"HAV1";
const MAX_SECRET: usize = 524_288;
const OVERHEAD: usize = 32;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const PRIVATE_FILE: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

/// AES-256-GCM file vault. Neither the DEK nor expanded cipher state is Debug.
pub struct EncryptedFileVault {
    root: File,
    dek: Zeroizing<[u8; 32]>,
}

impl EncryptedFileVault {
    /// Opens an existing private parent and creates only its final vault child.
    /// Every path component must already be a real directory (no symlinks).
    pub fn new(root: impl AsRef<Path>, dek: Zeroizing<[u8; 32]>) -> AccountsResult<Self> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(corrupt());
        }
        let parent = root.parent().ok_or_else(corrupt)?;
        let name = root.file_name().ok_or_else(corrupt)?;
        // The private parent stays readable for the mkdir durability barrier;
        // global ancestors are traversed through no-follow search handles.
        let parent =
            File::from(haider_platform::open_absolute_directory(parent).map_err(io_error)?);
        match rustix::fs::mkdirat(&parent, name, Mode::from_raw_mode(0o700)) {
            Ok(()) => rustix::fs::fsync(&parent).map_err(io_error)?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(io_error(error)),
        }
        let directory = File::from(
            rustix::fs::openat(&parent, name, DIRECTORY, Mode::empty()).map_err(io_error)?,
        );
        validate(&directory, true)?;
        Ok(Self {
            root: directory,
            dek,
        })
    }

    /// Authenticate every existing entry before reporting credential recovery.
    /// An empty vault provides no evidence about whether a key matches old data.
    pub fn authenticate_existing(&self) -> AccountsResult<()> {
        for alias in self.list()? {
            self.resolve(&alias)?;
        }
        Ok(())
    }

    fn cipher(&self) -> Aes256Gcm {
        // Both the retained key and cipher's expanded key/GHASH state zeroize.
        Aes256Gcm::new((&*self.dek).into())
    }

    fn filename(alias: &CredentialAlias) -> String {
        format!("{}.vault", hex::encode(alias.as_str()))
    }

    fn aad(alias: &CredentialAlias) -> AccountsResult<Vec<u8>> {
        let bytes = alias.as_str().as_bytes();
        let length = u32::try_from(bytes.len()).map_err(|_| corrupt())?;
        let mut aad = Vec::with_capacity(8 + bytes.len());
        aad.extend_from_slice(MAGIC);
        aad.extend_from_slice(&length.to_be_bytes());
        aad.extend_from_slice(bytes);
        Ok(aad)
    }

    fn open_entry(&self, name: &str) -> AccountsResult<File> {
        let file = File::from(
            rustix::fs::openat(
                &self.root,
                name,
                PRIVATE_FILE | OFlags::RDONLY,
                Mode::empty(),
            )
            .map_err(|error| {
                if error == rustix::io::Errno::NOENT {
                    accounts_error(ErrorCode::CredentialMissing, "credential is absent", false)
                } else {
                    io_error(error)
                }
            })?,
        );
        validate(&file, false)?;
        Ok(file)
    }

    fn sync(&self) -> AccountsResult<()> {
        self.root.sync_all().map_err(|_| corrupt())
    }
}

impl Vault for EncryptedFileVault {
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
        if secret.len() > MAX_SECRET {
            return Err(accounts_error(
                ErrorCode::InvalidArgument,
                "credential exceeds vault size limit",
                false,
            ));
        }
        let target = Self::filename(alias);
        // A wrong DEK or substituted/corrupt entry cannot silently be overwritten.
        match self.resolve(alias) {
            Ok(_) => {}
            Err(error) if error.code == ErrorCode::CredentialMissing => {}
            Err(error) => return Err(error),
        }
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| corrupt())?;
        let mut ciphertext = Zeroizing::new(secret.to_vec());
        let tag = self
            .cipher()
            .encrypt_inout_detached(
                (&nonce).into(),
                &Self::aad(alias)?,
                ciphertext.as_mut_slice().into(),
            )
            .map_err(|_| corrupt())?;
        let temp = format!(".hav1-{}.tmp", hex::encode(nonce));
        // Track ownership only AFTER exclusive creation. Never remove a collided
        // temporary belonging to another writer.
        let mut file = File::from(
            rustix::fs::openat(
                &self.root,
                temp.as_str(),
                PRIVATE_FILE | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                Mode::from_raw_mode(0o600),
            )
            .map_err(io_error)?,
        );
        let result = (|| {
            file.write_all(MAGIC)
                .and_then(|()| file.write_all(&nonce))
                .and_then(|()| file.write_all(&ciphertext))
                .and_then(|()| file.write_all(&tag))
                .and_then(|()| file.sync_all())
                .map_err(|_| corrupt())?;
            rustix::fs::renameat(&self.root, temp.as_str(), &self.root, target.as_str())
                .map_err(io_error)?;
            self.sync()
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&self.root, temp.as_str(), AtFlags::empty());
        }
        result
    }

    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        let file = self.open_entry(&Self::filename(alias))?;
        let length = file.metadata().map_err(|_| corrupt())?.len();
        if !(OVERHEAD as u64..=(MAX_SECRET + OVERHEAD) as u64).contains(&length) {
            return Err(corrupt());
        }
        let mut record = Zeroizing::new(Vec::with_capacity(length as usize));
        file.take((MAX_SECRET + OVERHEAD + 1) as u64)
            .read_to_end(&mut record)
            .map_err(|_| corrupt())?;
        if record.len() != length as usize || &record[..4] != MAGIC {
            return Err(corrupt());
        }
        let nonce: [u8; 12] = record[4..16].try_into().map_err(|_| corrupt())?;
        let tag: [u8; 16] = record[record.len() - 16..]
            .try_into()
            .map_err(|_| corrupt())?;
        let end = record.len() - 16;
        self.cipher()
            .decrypt_inout_detached(
                (&nonce).into(),
                &Self::aad(alias)?,
                (&mut record[16..end]).into(),
                (&tag).into(),
            )
            .map_err(|_| corrupt())?;
        let plaintext = Zeroizing::new(record[16..end].to_vec());
        Ok(SecretHandle::from_zeroizing(&plaintext))
    }

    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
        match self.resolve(alias) {
            Ok(_) => {}
            Err(error) if error.code == ErrorCode::CredentialMissing => return self.sync(),
            Err(error) => return Err(error),
        }
        match rustix::fs::unlinkat(&self.root, Self::filename(alias), AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => self.sync(),
            Err(error) => Err(io_error(error)),
        }
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        let entries = rustix::fs::Dir::read_from(&self.root).map_err(io_error)?;
        let mut aliases = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error)?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".vault") else {
                continue;
            };
            let bytes = hex::decode(stem).map_err(|_| corrupt())?;
            let alias = String::from_utf8(bytes).map_err(|_| corrupt())?;
            if hex::encode(&alias) != stem {
                return Err(corrupt());
            }
            self.open_entry(name)?;
            aliases.push(CredentialAlias::new(alias));
        }
        aliases.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(aliases)
    }

    fn try_refresh_lock(
        &self,
        alias: &CredentialAlias,
    ) -> AccountsResult<Option<VaultRefreshLock>> {
        let name = format!(".{}.refresh.lock", hex::encode(alias.as_str()));
        let file = File::from(
            rustix::fs::openat(
                &self.root,
                name,
                PRIVATE_FILE | OFlags::RDWR | OFlags::CREATE,
                Mode::from_raw_mode(0o600),
            )
            .map_err(io_error)?,
        );
        validate(&file, false)?;
        match haider_platform::try_lock_file_exclusive(&file) {
            Ok(()) => Ok(Some(VaultRefreshLock::new(move || drop(file)))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(_)) => Err(corrupt()),
        }
    }
}

fn validate(file: &File, directory: bool) -> AccountsResult<()> {
    let stat = rustix::fs::fstat(file).map_err(io_error)?;
    let expected_type = if directory {
        FileType::Directory
    } else {
        FileType::RegularFile
    };
    let expected_mode = if directory { 0o700 } else { 0o600 };
    if FileType::from_raw_mode(stat.st_mode) != expected_type
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != expected_mode
        || (!directory && stat.st_nlink != 1)
    {
        return Err(corrupt());
    }
    Ok(())
}

fn io_error(_: rustix::io::Errno) -> crate::HaiderError {
    corrupt()
}

fn corrupt() -> crate::HaiderError {
    accounts_error(
        ErrorCode::StoreCorrupt,
        "encrypted credential store is unavailable or corrupt",
        false,
    )
}
