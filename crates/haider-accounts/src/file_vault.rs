//! File-backed secret vault — the DEFAULT vault on every platform (W5f-4).
//!
//! R10 amendment (owner decision, 2026-07-30): the macOS Keychain default is
//! retired. Every haider build is ad-hoc-signed, so the Keychain saw a "new"
//! app each release and escalated to a login-keychain PASSWORD prompt from a
//! binary the user couldn't identify. The file vault stores each secret in
//! its own file under a daemon-owned directory with the same protection
//! model the peer CLIs use for their own tokens (`~/.codex/auth.json`,
//! `~/.claude/.credentials.json`): directory `0700`, files `0600`, owner
//! only. Secrets remain inside [`SecretHandle`] / zeroizing buffers in
//! memory, and never appear in errors — an alias is the only identifier a
//! message may carry.
//!
//! On-disk layout: `<root>/<hex(alias bytes)>.vault`. Hex is reversible (so
//! `list` recovers aliases without a side index), filesystem-safe, and
//! preserves byte order (so lexical filename order IS lexical alias order).
//! Writes are atomic: a `0600` temp file in the same directory, then rename.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;
use zeroize::Zeroizing;

use crate::vault::{SecretHandle, Vault, VaultRefreshLock};
use crate::{AccountsResult, accounts_error};

/// Largest secret the vault will store or read back — matches the OAuth
/// bundle ceiling with headroom; anything bigger is a corrupt write, not a
/// credential.
const MAX_SECRET_BYTES: u64 = 512 * 1024;

const VAULT_SUFFIX: &str = ".vault";
const REFRESH_LOCK_SUFFIX: &str = ".refresh.lock";

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// File-per-alias vault rooted at a daemon-owned directory.
pub struct FileVault {
    root: PathBuf,
}

impl FileVault {
    /// A vault rooted at `root`. The directory is created (mode `0700`) on
    /// first write; a read-only operation on a missing root behaves as an
    /// empty vault.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, alias: &CredentialAlias) -> PathBuf {
        self.root
            .join(format!("{}{VAULT_SUFFIX}", hex::encode(alias.as_str())))
    }

    fn refresh_lock_path_for(&self, alias: &CredentialAlias) -> PathBuf {
        self.root.join(format!(
            ".{}{REFRESH_LOCK_SUFFIX}",
            hex::encode(alias.as_str())
        ))
    }

    fn ensure_root(&self) -> AccountsResult<()> {
        if self.root.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|error| io_error("create vault directory", &error))?;
        haider_platform::set_mode(&self.root, 0o700)
            .map_err(|error| io_error("restrict vault directory", &error))?;
        Ok(())
    }
}

impl Vault for FileVault {
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
        if u64::try_from(secret.len()).unwrap_or(u64::MAX) > MAX_SECRET_BYTES {
            return Err(accounts_error(
                ErrorCode::InvalidArgument,
                format!("secret for alias `{alias}` exceeds the vault size limit"),
                false,
            ));
        }
        self.ensure_root()?;
        let target = self.path_for(alias);
        // Atomic replace: same-directory temp file, restrictive from birth,
        // then rename over the target. A crash leaves either the old secret
        // or the new one, never a torn file — and the temp name carries the
        // hex alias (public), never secret material.
        let temp = self.root.join(format!(
            ".{}.tmp-{}-{}",
            hex::encode(alias.as_str()),
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed),
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        haider_platform::configure_file_mode(&mut options, 0o600);
        let result = (|| {
            let mut file = options
                .open(&temp)
                .map_err(|error| io_error("stage vault write", &error))?;
            file.write_all(secret)
                .and_then(|()| file.sync_all())
                .map_err(|error| io_error("write vault secret", &error))?;
            drop(file);
            commit_temp(&temp, &target).map_err(|error| io_error("commit vault secret", &error))?;
            sync_directory(&self.root).map_err(|error| io_error("sync vault directory", &error))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        let path = self.path_for(alias);
        let metadata = std::fs::metadata(&path).map_err(|_| missing(alias))?;
        if metadata.len() > MAX_SECRET_BYTES {
            return Err(accounts_error(
                ErrorCode::StoreCorrupt,
                format!("vault entry for alias `{alias}` is oversized"),
                false,
            ));
        }
        let bytes = Zeroizing::new(
            std::fs::read(&path).map_err(|error| io_error("read vault secret", &error))?,
        );
        Ok(SecretHandle::from_zeroizing(&bytes))
    }

    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
        match std::fs::remove_file(self.path_for(alias)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("delete vault secret", &error)),
        }
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error("list vault directory", &error)),
        };
        let mut aliases = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| io_error("list vault directory", &error))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Anything that is not `<hex>.vault` — temp files, strays — is
            // not vault data and is silently skipped, never an error.
            let Some(stem) = name.strip_suffix(VAULT_SUFFIX) else {
                continue;
            };
            let Ok(decoded) = hex::decode(stem) else {
                continue;
            };
            let Ok(alias) = String::from_utf8(decoded) else {
                continue;
            };
            aliases.push(CredentialAlias::new(alias));
        }
        aliases.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(aliases)
    }

    fn try_refresh_lock(
        &self,
        alias: &CredentialAlias,
    ) -> AccountsResult<Option<VaultRefreshLock>> {
        self.ensure_root()?;
        let path = self.refresh_lock_path_for(alias);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        haider_platform::configure_file_mode(&mut options, 0o600);
        let file = options
            .open(path)
            .map_err(|error| io_error("open vault refresh lock", &error))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(VaultRefreshLock::new(move || drop(file)))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(io_error("acquire vault refresh lock", &error))
            }
        }
    }
}

#[cfg(unix)]
fn commit_temp(temp: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(temp, target)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn commit_temp(temp: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let temp = temp
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            temp.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    haider_platform::sync_directory(path)
}

#[cfg(windows)]
fn sync_directory(_path: &std::path::Path) -> std::io::Result<()> {
    // MOVEFILE_WRITE_THROUGH above flushes the replacement before returning.
    Ok(())
}

fn missing(alias: &CredentialAlias) -> haider_protocol::error::HaiderError {
    accounts_error(
        ErrorCode::CredentialMissing,
        format!("credential alias `{alias}` is not in the vault"),
        false,
    )
}

fn io_error(operation: &str, error: &std::io::Error) -> haider_protocol::error::HaiderError {
    accounts_error(
        ErrorCode::Internal,
        // The io::Error's own message never contains secret bytes: every
        // operation name here describes vault plumbing, and paths carry hex
        // aliases only.
        format!("could not {operation}: {error}"),
        error.kind() == std::io::ErrorKind::Interrupted,
    )
}
