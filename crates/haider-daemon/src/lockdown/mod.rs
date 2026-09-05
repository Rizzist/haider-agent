use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use haider_platform::{SyncPolicy, configure_directory_mode, configure_file_mode, set_mode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const DEFAULT_QUOTA_BYTES: u64 = 1024 * 1024 * 1024;
#[cfg(not(test))]
const LOCKDOWN_DIRECTORY: &str = "lockdown";
const QUOTA_FILE: &str = "quota.json";
const QUOTA_LOCK_FILE: &str = "quota.lock";
const QUOTA_TEMP_FILE: &str = "quota.tmp";
const TURN_BINDINGS_FILE: &str = "turns.json";
const TURN_BINDINGS_TEMP_FILE: &str = "turns.tmp";
const DATA_TEMP_PREFIX: &str = ".ld-";
const DATA_TEMP_NAME_BYTES: usize = 20;
const MAX_SANDBOX_PATH_BYTES: usize = 240;
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const LEDGER_VERSION: u32 = 1;
static GLOBAL_MANAGER: OnceLock<LockdownManager> = OnceLock::new();

pub(crate) const ALLOWED_TOOLS: &[&str] = &[
    "fs_read",
    "fs_glob",
    "fs_search",
    "fs_write",
    "request_input",
    "todo_write",
    "plan",
    "web_search",
    "web_fetch",
    "peer_list",
    "ssh_list",
    "spawn_subagent",
    "list_models",
];

#[derive(Debug)]
pub(crate) enum LockdownError {
    AppliedWrite(Box<LockdownError>),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidProviderName,
    InvalidRelativePath {
        path: PathBuf,
    },
    PathTooLong {
        length: usize,
        limit: usize,
        path: PathBuf,
    },
    SymlinkRefused {
        path: PathBuf,
    },
    LockdownQuotaExceeded {
        used: u64,
        limit: u64,
    },
    QuotaCommandConflict {
        command_id: String,
    },
    InvalidLedger {
        path: PathBuf,
        reason: String,
    },
    TurnBindingConflict {
        session_id: String,
        run_id: String,
        stored_provider: String,
        requested_provider: String,
    },
}

impl fmt::Display for LockdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AppliedWrite(source) => fmt::Display::fmt(source, formatter),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "cannot {operation} {}: {source}", path.display()),
            Self::InvalidProviderName => formatter.write_str("provider name is empty"),
            Self::InvalidRelativePath { path } => write!(
                formatter,
                "lockdown path must be relative and remain inside the provider sandbox: {}",
                path.display()
            ),
            Self::PathTooLong {
                length,
                limit,
                path,
            } => write!(
                formatter,
                "lockdown path is {length} bytes, exceeding the {limit}-byte limit: {}",
                path.display()
            ),
            Self::SymlinkRefused { path } => write!(
                formatter,
                "lockdown path contains or targets a symbolic link: {}",
                path.display()
            ),
            Self::LockdownQuotaExceeded { used, limit } => write!(
                formatter,
                "LockdownQuotaExceeded {{ used: {used}, limit: {limit} }}"
            ),
            Self::QuotaCommandConflict { command_id } => write!(
                formatter,
                "lockdown quota command `{command_id}` was already used with different bytes"
            ),
            Self::InvalidLedger { path, reason } => write!(
                formatter,
                "invalid lockdown quota ledger {}: {reason}",
                path.display()
            ),
            Self::TurnBindingConflict {
                session_id,
                run_id,
                stored_provider,
                requested_provider,
            } => write!(
                formatter,
                "lockdown turn binding conflict for {session_id}/{run_id}: stored provider `{stored_provider}`, requested `{requested_provider}`"
            ),
        }
    }
}

impl std::error::Error for LockdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockdownStatus {
    pub(crate) provider: Option<String>,
    pub(crate) sandbox: Option<PathBuf>,
    pub(crate) tools_allowed: Vec<String>,
    pub(crate) quota_used: u64,
    pub(crate) quota_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockdownTurn {
    pub(crate) provider: String,
    pub(crate) sandbox: PathBuf,
    pub(crate) tools_allowed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuotaLedger {
    version: u32,
    limit: u64,
    used: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    commands: Vec<QuotaCommandReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuotaCommandReceipt {
    command_id: String,
    bytes: u64,
    #[serde(default)]
    used: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnBindingLedger {
    version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    active: Vec<DurableTurnBinding>,
    bindings: Vec<DurableTurnBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableTurnBinding {
    profile_id: String,
    session_id: String,
    run_id: String,
    provider: String,
    lockdown: bool,
    /// Additive exact-policy bit. Old ledgers decode as ordinary configured
    /// lockdown; new auto-hermetic bindings must remain strict after restart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    auto_hermetic: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LockdownManager {
    root: PathBuf,
}

impl LockdownManager {
    pub(crate) fn initialize_default() -> Result<Self, LockdownError> {
        #[cfg(test)]
        {
            Self::initialize(std::env::temp_dir().join(format!("haider-ld-{}", std::process::id())))
        }
        #[cfg(not(test))]
        let home = crate::oauth::oauth_home_dir().ok_or_else(|| LockdownError::InvalidLedger {
            path: PathBuf::from("~"),
            reason: "HOME (or USERPROFILE on Windows) is not set".to_owned(),
        })?;
        #[cfg(not(test))]
        let haider_root = PathBuf::from(home).join(".haider");
        #[cfg(not(test))]
        let lockdown_root = haider_root.join(LOCKDOWN_DIRECTORY);
        #[cfg(not(test))]
        check_path_budget(&lockdown_root)?;
        #[cfg(not(test))]
        ensure_private_directory(&haider_root)?;
        #[cfg(not(test))]
        Self::initialize(lockdown_root)
    }

    pub(crate) fn initialize(root: PathBuf) -> Result<Self, LockdownError> {
        check_root_budget(&root)?;
        ensure_private_directory(&root)?;
        let manager = Self { root };
        manager.with_locked_ledger(|ledger| {
            ledger.used = directory_size(&manager.root)?;
            Ok(())
        })?;
        Ok(manager)
    }

    pub(crate) fn status(&self, provider: Option<&str>) -> Result<LockdownStatus, LockdownError> {
        let sandbox = provider.map(|name| self.provider_root(name)).transpose()?;
        self.with_locked_ledger(|ledger| {
            ledger.used = directory_size(&self.root)?;
            Ok(LockdownStatus {
                provider: provider.map(str::to_owned),
                sandbox,
                tools_allowed: allowed_tool_names(),
                quota_used: ledger.used,
                quota_limit: ledger.limit,
            })
        })
    }

    pub(crate) fn turn(&self, provider: &str) -> Result<LockdownTurn, LockdownError> {
        let status = self.status(Some(provider))?;
        let sandbox = status.sandbox.ok_or(LockdownError::InvalidProviderName)?;
        Ok(LockdownTurn {
            provider: provider.to_owned(),
            sandbox,
            tools_allowed: status.tools_allowed,
        })
    }

    pub(crate) fn bind_turn(
        &self,
        profile_id: &str,
        session_id: &str,
        run_id: &str,
        provider: &str,
        proposed_lockdown: bool,
        proposed_auto_hermetic: bool,
    ) -> Result<(String, bool, bool), LockdownError> {
        self.with_global_lock(|| {
            let mut ledger = self.load_turn_bindings()?;
            if let Some(index) = ledger.bindings.iter().position(|binding| {
                binding.profile_id == profile_id
                    && binding.session_id == session_id
                    && binding.run_id == run_id
            }) {
                let binding = &mut ledger.bindings[index];
                if binding.provider != provider {
                    return Err(LockdownError::TurnBindingConflict {
                        session_id: session_id.to_owned(),
                        run_id: run_id.to_owned(),
                        stored_provider: binding.provider.clone(),
                        requested_provider: provider.to_owned(),
                    });
                }
                // The exact active-account fact can arrive after an older
                // manifest or observer installed ordinary lockdown. Allow
                // only this monotonic narrowing; Full or Configured can
                // never replace an already-auto-hermetic binding.
                if proposed_auto_hermetic && !binding.auto_hermetic {
                    binding.lockdown = true;
                    binding.auto_hermetic = true;
                    let result = (
                        binding.provider.clone(),
                        binding.lockdown,
                        binding.auto_hermetic,
                    );
                    self.persist_turn_bindings(&ledger)?;
                    return Ok(result);
                }
                return Ok((
                    binding.provider.clone(),
                    binding.lockdown,
                    binding.auto_hermetic,
                ));
            }
            ledger.bindings.push(DurableTurnBinding {
                profile_id: profile_id.to_owned(),
                session_id: session_id.to_owned(),
                run_id: run_id.to_owned(),
                provider: provider.to_owned(),
                lockdown: proposed_lockdown,
                auto_hermetic: proposed_auto_hermetic,
            });
            self.persist_turn_bindings(&ledger)?;
            Ok((
                provider.to_owned(),
                proposed_lockdown,
                proposed_auto_hermetic,
            ))
        })
    }

    pub(crate) fn turn_binding(
        &self,
        profile_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<(String, bool, bool)>, LockdownError> {
        self.with_global_lock(|| {
            Ok(self
                .load_turn_bindings()?
                .bindings
                .into_iter()
                .find(|binding| {
                    binding.profile_id == profile_id
                        && binding.session_id == session_id
                        && binding.run_id == run_id
                })
                .map(|binding| (binding.provider, binding.lockdown, binding.auto_hermetic)))
        })
    }

    pub(crate) fn latest_session_provider(
        &self,
        profile_id: &str,
        session_id: &str,
    ) -> Result<Option<String>, LockdownError> {
        self.with_global_lock(|| {
            Ok(self
                .load_turn_bindings()?
                .active
                .into_iter()
                .rev()
                .find(|binding| {
                    binding.profile_id == profile_id && binding.session_id == session_id
                })
                .map(|binding| binding.provider))
        })
    }

    pub(crate) fn activate_turn(
        &self,
        profile_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<(), LockdownError> {
        self.with_global_lock(|| {
            let mut ledger = self.load_turn_bindings()?;
            let binding = ledger
                .bindings
                .iter()
                .find(|binding| {
                    binding.profile_id == profile_id
                        && binding.session_id == session_id
                        && binding.run_id == run_id
                })
                .cloned()
                .ok_or_else(|| LockdownError::InvalidLedger {
                    path: self.root.join(TURN_BINDINGS_FILE),
                    reason: format!(
                        "turn {session_id}/{run_id} was activated before its durable binding"
                    ),
                })?;
            ledger.active.retain(|active| {
                active.profile_id != profile_id || active.session_id != session_id
            });
            ledger.active.push(binding);
            self.persist_turn_bindings(&ledger)
        })
    }

    pub(crate) fn active_session_binding(
        &self,
        profile_id: &str,
        session_id: &str,
    ) -> Result<Option<(String, String, bool, bool)>, LockdownError> {
        self.with_global_lock(|| {
            Ok(self
                .load_turn_bindings()?
                .active
                .into_iter()
                .rev()
                .find(|binding| {
                    binding.profile_id == profile_id && binding.session_id == session_id
                })
                .map(|binding| {
                    (
                        binding.run_id,
                        binding.provider,
                        binding.lockdown,
                        binding.auto_hermetic,
                    )
                }))
        })
    }

    pub(crate) fn remove_session_bindings(
        &self,
        profile_id: &str,
        session_id: &str,
    ) -> Result<(), LockdownError> {
        self.with_global_lock(|| {
            let mut ledger = self.load_turn_bindings()?;
            let bindings_before = ledger.bindings.len();
            let active_before = ledger.active.len();
            ledger.bindings.retain(|binding| {
                binding.profile_id != profile_id || binding.session_id != session_id
            });
            ledger.active.retain(|binding| {
                binding.profile_id != profile_id || binding.session_id != session_id
            });
            if ledger.bindings.len() != bindings_before || ledger.active.len() != active_before {
                self.persist_turn_bindings(&ledger)?;
            }
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn set_quota(&self, limit: u64) -> Result<LockdownStatus, LockdownError> {
        self.set_quota_inner(None, limit)
    }

    pub(crate) fn set_quota_command(
        &self,
        command_id: &str,
        limit: u64,
    ) -> Result<LockdownStatus, LockdownError> {
        self.set_quota_inner(Some(command_id), limit)
    }

    fn set_quota_inner(
        &self,
        command_id: Option<&str>,
        limit: u64,
    ) -> Result<LockdownStatus, LockdownError> {
        self.with_locked_ledger(|ledger| {
            ledger.used = directory_size(&self.root)?;
            if let Some(command_id) = command_id
                && let Some(receipt) = ledger
                    .commands
                    .iter()
                    .find(|receipt| receipt.command_id == command_id)
            {
                if receipt.bytes != limit {
                    return Err(LockdownError::QuotaCommandConflict {
                        command_id: command_id.to_owned(),
                    });
                }
                return Ok(LockdownStatus {
                    provider: None,
                    sandbox: None,
                    tools_allowed: allowed_tool_names(),
                    quota_used: receipt.used,
                    quota_limit: receipt.bytes,
                });
            }
            if ledger.used > limit {
                return Err(LockdownError::LockdownQuotaExceeded {
                    used: ledger.used,
                    limit,
                });
            }
            ledger.limit = limit;
            if let Some(command_id) = command_id {
                ledger.commands.push(QuotaCommandReceipt {
                    command_id: command_id.to_owned(),
                    bytes: limit,
                    used: ledger.used,
                });
            }
            Ok(lockdown_status(ledger))
        })
    }

    pub(crate) fn write(
        &self,
        provider: &str,
        relative_path: &Path,
        contents: &[u8],
    ) -> Result<LockdownStatus, LockdownError> {
        self.write_with_post_apply(provider, relative_path, contents, || Ok(()))
    }

    fn write_with_post_apply(
        &self,
        provider: &str,
        relative_path: &Path,
        contents: &[u8],
        after_apply: impl FnOnce() -> Result<(), LockdownError>,
    ) -> Result<LockdownStatus, LockdownError> {
        let provider_root = self.provider_root(provider)?;
        let target = sandbox_path(&provider_root, relative_path)?;
        let temporary_name = data_temporary_name(&target)?;
        check_path_budget(&self.root.join(&temporary_name))?;
        let mut applied = false;
        let result = self.with_locked_ledger(|ledger| {
            let parent = target
                .parent()
                .ok_or_else(|| LockdownError::InvalidRelativePath {
                    path: relative_path.to_path_buf(),
                })?;
            refuse_existing_symlink_ancestors(&provider_root, &target)?;

            ledger.used = directory_size(&self.root)?;
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => {
                    return Err(LockdownError::InvalidRelativePath {
                        path: relative_path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(io_error("inspect", &target, source)),
            }
            // The private staging file coexists with the old target until
            // atomic replacement. Charge that physical peak, not only the
            // final logical tree, so a same-size replacement at the limit
            // cannot transiently double the machine-user cap.
            let staged_peak = ledger
                .used
                .saturating_add(u64::try_from(contents.len()).unwrap_or(u64::MAX));
            if staged_peak > ledger.limit {
                return Err(LockdownError::LockdownQuotaExceeded {
                    used: ledger.used,
                    limit: ledger.limit,
                });
            }

            ensure_private_directory(&provider_root)?;
            ensure_private_descendants(&provider_root, parent)?;
            refuse_symlink(&target)?;
            atomic_write_staged(
                &self.root,
                &temporary_name,
                &target,
                contents,
                &mut applied,
                after_apply,
            )?;
            ledger.used = directory_size(&self.root)?;
            Ok(LockdownStatus {
                provider: Some(provider.to_owned()),
                sandbox: Some(provider_root.clone()),
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

    pub(crate) fn read(
        &self,
        provider: &str,
        relative_path: &Path,
    ) -> Result<Vec<u8>, LockdownError> {
        let provider_root = self.provider_root(provider)?;
        refuse_symlink(&provider_root)?;
        let target = if relative_path.as_os_str().is_empty() {
            provider_root.clone()
        } else {
            sandbox_path(&provider_root, relative_path)?
        };
        refuse_symlink_ancestors(&provider_root, &target)?;
        let metadata =
            fs::symlink_metadata(&target).map_err(|source| io_error("inspect", &target, source))?;
        if metadata.is_dir() {
            let mut entries = fs::read_dir(&target)
                .map_err(|source| io_error("list", &target, source))?
                .take(MAX_DIRECTORY_ENTRIES)
                .map(|entry| {
                    let entry = entry.map_err(|source| io_error("list", &target, source))?;
                    let path = entry.path();
                    let metadata = entry
                        .file_type()
                        .map_err(|source| io_error("inspect", &path, source))?;
                    if metadata.is_symlink() {
                        return Err(LockdownError::SymlinkRefused { path });
                    }
                    let mut name = entry.file_name().to_string_lossy().into_owned();
                    if metadata.is_dir() {
                        name.push('/');
                    }
                    Ok(name)
                })
                .collect::<Result<Vec<_>, LockdownError>>()?;
            entries.sort();
            return Ok(entries.join("\n").into_bytes());
        }
        let mut file = File::open(&target).map_err(|source| io_error("open", &target, source))?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .map_err(|source| io_error("read", &target, source))?;
        Ok(contents)
    }

    pub(crate) fn provider_root(&self, provider: &str) -> Result<PathBuf, LockdownError> {
        let slug = provider_slug(provider)?;
        let path = self.root.join(slug);
        check_path_budget(&path)?;
        Ok(path)
    }

    pub(crate) fn sandbox_location(
        &self,
        provider: &str,
        relative_path: &Path,
    ) -> Result<PathBuf, LockdownError> {
        sandbox_path(&self.provider_root(provider)?, relative_path)
    }

    fn with_locked_ledger<T>(
        &self,
        operation: impl FnOnce(&mut QuotaLedger) -> Result<T, LockdownError>,
    ) -> Result<T, LockdownError> {
        self.with_global_lock(|| {
            let (mut ledger, existing_bytes) = self.load_ledger()?;
            let value = operation(&mut ledger)?;
            self.persist_ledger_if_changed(&ledger, existing_bytes.as_deref())?;
            Ok(value)
        })
    }

    fn with_global_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, LockdownError>,
    ) -> Result<T, LockdownError> {
        let lock_path = self.root.join(QUOTA_LOCK_FILE);
        let lock = private_open(&lock_path, true)?;
        lock.lock()
            .map_err(|source| io_error("lock", &lock_path, source))?;
        cleanup_stale_temporaries(&self.root)?;
        let result = operation();
        let unlocked = lock
            .unlock()
            .map_err(|source| io_error("unlock", &lock_path, source));
        match result {
            Err(error) => Err(error),
            Ok(value) => {
                unlocked?;
                Ok(value)
            }
        }
    }

    fn load_ledger(&self) -> Result<(QuotaLedger, Option<Vec<u8>>), LockdownError> {
        let path = self.root.join(QUOTA_FILE);
        refuse_symlink(&path)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|source| io_error("inspect", &path, source))?;
                if !metadata.is_file() {
                    return Err(LockdownError::InvalidLedger {
                        path,
                        reason: "quota ledger is not a regular file".to_owned(),
                    });
                }
                set_mode(&path, 0o600).map_err(|source| io_error("set mode on", &path, source))?;
                let ledger: QuotaLedger = serde_json::from_slice(&bytes).map_err(|error| {
                    LockdownError::InvalidLedger {
                        path: path.clone(),
                        reason: error.to_string(),
                    }
                })?;
                if ledger.version != LEDGER_VERSION {
                    return Err(LockdownError::InvalidLedger {
                        path,
                        reason: format!("unsupported version {}", ledger.version),
                    });
                }
                Ok((ledger, Some(bytes)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((
                QuotaLedger {
                    version: LEDGER_VERSION,
                    limit: DEFAULT_QUOTA_BYTES,
                    used: 0,
                    commands: Vec::new(),
                },
                None,
            )),
            Err(source) => Err(io_error("read", &path, source)),
        }
    }

    fn persist_ledger_if_changed(
        &self,
        ledger: &QuotaLedger,
        existing_bytes: Option<&[u8]>,
    ) -> Result<(), LockdownError> {
        let bytes =
            serde_json::to_vec_pretty(ledger).map_err(|error| LockdownError::InvalidLedger {
                path: self.root.join(QUOTA_FILE),
                reason: error.to_string(),
            })?;
        if existing_bytes == Some(bytes.as_slice()) {
            return Ok(());
        }
        atomic_write_named(&self.root, QUOTA_TEMP_FILE, QUOTA_FILE, &bytes)
    }

    fn load_turn_bindings(&self) -> Result<TurnBindingLedger, LockdownError> {
        let path = self.root.join(TURN_BINDINGS_FILE);
        refuse_symlink(&path)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|source| io_error("inspect", &path, source))?;
                if !metadata.is_file() {
                    return Err(LockdownError::InvalidLedger {
                        path,
                        reason: "turn-binding ledger is not a regular file".to_owned(),
                    });
                }
                set_mode(&path, 0o600).map_err(|source| io_error("set mode on", &path, source))?;
                let ledger: TurnBindingLedger =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        LockdownError::InvalidLedger {
                            path: path.clone(),
                            reason: error.to_string(),
                        }
                    })?;
                if ledger.version != LEDGER_VERSION {
                    return Err(LockdownError::InvalidLedger {
                        path,
                        reason: format!("unsupported turn-binding version {}", ledger.version),
                    });
                }
                Ok(ledger)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TurnBindingLedger {
                version: LEDGER_VERSION,
                active: Vec::new(),
                bindings: Vec::new(),
            }),
            Err(source) => Err(io_error("read", &path, source)),
        }
    }

    fn persist_turn_bindings(&self, ledger: &TurnBindingLedger) -> Result<(), LockdownError> {
        let bytes =
            serde_json::to_vec_pretty(ledger).map_err(|error| LockdownError::InvalidLedger {
                path: self.root.join(TURN_BINDINGS_FILE),
                reason: error.to_string(),
            })?;
        atomic_write_named(
            &self.root,
            TURN_BINDINGS_TEMP_FILE,
            TURN_BINDINGS_FILE,
            &bytes,
        )
    }
}

fn lockdown_status(ledger: &QuotaLedger) -> LockdownStatus {
    LockdownStatus {
        provider: None,
        sandbox: None,
        tools_allowed: allowed_tool_names(),
        quota_used: ledger.used,
        quota_limit: ledger.limit,
    }
}

pub(crate) fn initialize_global(
    root: Option<&Path>,
) -> Result<&'static LockdownManager, LockdownError> {
    if let Some(manager) = GLOBAL_MANAGER.get() {
        return Ok(manager);
    }
    let manager = match root {
        Some(root) => LockdownManager::initialize(root.to_path_buf())?,
        None => LockdownManager::initialize_default()?,
    };
    let _ = GLOBAL_MANAGER.set(manager);
    GLOBAL_MANAGER
        .get()
        .ok_or_else(|| LockdownError::InvalidLedger {
            path: PathBuf::from("~/.haider/lockdown"),
            reason: "global lockdown manager was not installed".to_owned(),
        })
}

pub(crate) fn global() -> Result<&'static LockdownManager, LockdownError> {
    GLOBAL_MANAGER
        .get()
        .ok_or_else(|| LockdownError::InvalidLedger {
            path: PathBuf::from("~/.haider/lockdown"),
            reason: "global lockdown manager is unavailable before daemon startup".to_owned(),
        })
}

/// Returns the process-global manager only after daemon startup installed it.
/// Read-only projections use this door so constructing an in-process hub does
/// not open machine-user-global state as a side effect.
pub(crate) fn global_if_initialized() -> Option<&'static LockdownManager> {
    GLOBAL_MANAGER.get()
}

pub(crate) fn allowed_tool_names() -> Vec<String> {
    ALLOWED_TOOLS
        .iter()
        .map(|tool| (*tool).to_owned())
        .collect()
}

pub(crate) fn tool_allowed(tool: &str) -> bool {
    ALLOWED_TOOLS.contains(&tool)
}

pub(crate) fn read_path_allowed(workspace: &Path, sandbox: &Path, requested: &Path) -> bool {
    let absolute = if requested.is_absolute() {
        normalize_lexically(requested)
    } else {
        normalize_lexically(&workspace.join(requested))
    };
    if !path_is_in_read_scope(workspace, sandbox, &absolute) {
        return false;
    }
    // The filesystem tools perform their own handle-anchored boundary
    // checks, but the lockdown ceiling must independently catch an in-scope
    // symlink whose resolved target is a profile, vault, environment, or SSH
    // path. A missing target cannot disclose bytes and is left to the typed
    // filesystem error below this layer.
    match absolute.canonicalize() {
        Ok(resolved) => path_is_in_read_scope(workspace, sandbox, &resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn path_is_in_read_scope(workspace: &Path, sandbox: &Path, path: &Path) -> bool {
    path.starts_with(sandbox) || (path.starts_with(workspace) && !sensitive_read_path(path))
}

pub(crate) fn sensitive_read_path(path: &Path) -> bool {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_ascii_lowercase),
            _ => None,
        })
        .collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair == [".config", "gcloud"])
        || components.iter().any(|component| {
            component == ".env"
                || component.starts_with(".env.")
                || component.starts_with("id_rsa")
                || component.starts_with("credentials")
                || matches!(
                    component.as_str(),
                    ".ssh"
                        | ".aws"
                        | ".gnupg"
                        | ".kube"
                        | ".azure"
                        | ".haider"
                        | ".netrc"
                        | ".npmrc"
                        | ".pypirc"
                        | "vault"
                        | "vault.json"
                        | "providers.json"
                        | "profile.json"
                )
                || ["pem", "key", "p12", "jks", "keystore", "tfstate"]
                    .iter()
                    .any(|extension| component.ends_with(&format!(".{extension}")))
        })
}

pub(crate) fn provider_slug(provider: &str) -> Result<String, LockdownError> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Err(LockdownError::InvalidProviderName);
    }
    let digest = format!("{:x}", Sha256::digest(provider.as_bytes()));
    // Twenty hex characters preserve an 80-bit collision margin while obeying the
    // repository-wide runtime/sandbox basename ceiling.
    Ok(digest.get(..20).unwrap_or(digest.as_str()).to_owned())
}

fn sandbox_path(root: &Path, relative: &Path) -> Result<PathBuf, LockdownError> {
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(LockdownError::InvalidRelativePath {
            path: relative.to_path_buf(),
        });
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(LockdownError::InvalidRelativePath {
            path: relative.to_path_buf(),
        });
    }
    let path = root.join(relative);
    check_path_budget(&path)?;
    Ok(path)
}

fn check_path_budget(path: &Path) -> Result<(), LockdownError> {
    let length = path.as_os_str().as_encoded_bytes().len();
    if length > MAX_SANDBOX_PATH_BYTES {
        return Err(LockdownError::PathTooLong {
            length,
            limit: MAX_SANDBOX_PATH_BYTES,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn check_root_budget(root: &Path) -> Result<(), LockdownError> {
    check_path_budget(root)?;
    for child in [
        QUOTA_FILE,
        QUOTA_LOCK_FILE,
        QUOTA_TEMP_FILE,
        TURN_BINDINGS_FILE,
        TURN_BINDINGS_TEMP_FILE,
        // Twenty digest characters is the longest provider basename produced by
        // `provider_slug`.
        "00000000000000000000",
    ] {
        check_path_budget(&root.join(child))?;
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), LockdownError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(LockdownError::SymlinkRefused {
                path: path.to_path_buf(),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(LockdownError::InvalidRelativePath {
                path: path.to_path_buf(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true);
            configure_directory_mode(&mut builder, 0o700);
            builder
                .create(path)
                .map_err(|source| io_error("create directory", path, source))?;
        }
        Err(source) => return Err(io_error("inspect", path, source)),
    }
    set_mode(path, 0o700).map_err(|source| io_error("set mode on", path, source))
}

fn ensure_private_descendants(root: &Path, target: &Path) -> Result<(), LockdownError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| LockdownError::InvalidRelativePath {
            path: target.to_path_buf(),
        })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(LockdownError::InvalidRelativePath {
                path: target.to_path_buf(),
            });
        };
        current.push(name);
        ensure_private_directory(&current)?;
    }
    Ok(())
}

fn refuse_symlink(path: &Path) -> Result<(), LockdownError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(LockdownError::SymlinkRefused {
            path: path.to_path_buf(),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("inspect", path, source)),
    }
}

fn refuse_symlink_ancestors(root: &Path, target: &Path) -> Result<(), LockdownError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| LockdownError::InvalidRelativePath {
            path: target.to_path_buf(),
        })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        refuse_symlink(&current)?;
    }
    Ok(())
}

fn refuse_existing_symlink_ancestors(root: &Path, target: &Path) -> Result<(), LockdownError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| LockdownError::InvalidRelativePath {
            path: target.to_path_buf(),
        })?;
    let mut current = root.to_path_buf();
    for component in std::iter::once(Component::CurDir).chain(relative.components()) {
        if !matches!(component, Component::CurDir) {
            current.push(component.as_os_str());
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LockdownError::SymlinkRefused { path: current });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_error("inspect", &current, source)),
        }
    }
    Ok(())
}

fn private_open(path: &Path, create: bool) -> Result<File, LockdownError> {
    refuse_symlink(path)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false);
    configure_file_mode(&mut options, 0o600);
    let file = options
        .open(path)
        .map_err(|source| io_error("open", path, source))?;
    let metadata = file
        .metadata()
        .map_err(|source| io_error("inspect", path, source))?;
    if !metadata.is_file() {
        return Err(LockdownError::InvalidLedger {
            path: path.to_path_buf(),
            reason: "lock path is not a regular file".to_owned(),
        });
    }
    set_mode(path, 0o600).map_err(|source| io_error("set mode on", path, source))?;
    Ok(file)
}

fn data_temporary_name(target: &Path) -> Result<String, LockdownError> {
    let sequence = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let digest = format!(
        "{:x}",
        Sha256::digest(
            format!("{}\0{}\0{sequence}", target.display(), std::process::id()).as_bytes()
        )
    );
    let suffix = digest
        .get(..DATA_TEMP_NAME_BYTES - DATA_TEMP_PREFIX.len())
        .ok_or_else(|| LockdownError::InvalidLedger {
            path: target.to_path_buf(),
            reason: "temporary-name digest is unexpectedly short".to_owned(),
        })?;
    Ok(format!("{DATA_TEMP_PREFIX}{suffix}"))
}

fn atomic_write_staged(
    staging_root: &Path,
    temporary_name: &str,
    target: &Path,
    contents: &[u8],
    applied: &mut bool,
    after_apply: impl FnOnce() -> Result<(), LockdownError>,
) -> Result<(), LockdownError> {
    let parent = target
        .parent()
        .ok_or_else(|| LockdownError::InvalidRelativePath {
            path: target.to_path_buf(),
        })?;
    let temporary = staging_root.join(temporary_name);
    check_path_budget(&temporary)?;
    check_path_budget(target)?;
    write_and_replace(&temporary, target, contents)?;
    *applied = true;
    after_apply()?;
    haider_platform::fs::sync_directory(parent, SyncPolicy::Full)
        .map_err(|source| io_error("sync directory", parent, source))?;
    if parent != staging_root {
        haider_platform::fs::sync_directory(staging_root, SyncPolicy::Full)
            .map_err(|source| io_error("sync directory", staging_root, source))?;
    }
    Ok(())
}

fn atomic_write_named(
    parent: &Path,
    temporary_name: &str,
    target_name: &str,
    contents: &[u8],
) -> Result<(), LockdownError> {
    let temporary = parent.join(temporary_name);
    let target = parent.join(target_name);
    check_path_budget(&temporary)?;
    check_path_budget(&target)?;
    write_and_replace(&temporary, &target, contents)?;
    haider_platform::fs::sync_directory(parent, SyncPolicy::Full)
        .map_err(|source| io_error("sync directory", parent, source))
}

fn write_and_replace(
    temporary: &Path,
    target: &Path,
    contents: &[u8],
) -> Result<(), LockdownError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    configure_file_mode(&mut options, 0o600);
    let mut file = options
        .open(temporary)
        .map_err(|source| io_error("create", temporary, source))?;
    let write_result = (|| {
        file.write_all(contents)
            .map_err(|source| io_error("write", temporary, source))?;
        haider_platform::sync_file(&file, SyncPolicy::Full)
            .map_err(|source| io_error("sync", temporary, source))?;
        drop(file);
        haider_platform::replace_file(temporary, target)
            .map_err(|source| io_error("replace", target, source))
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    write_result
}

fn cleanup_stale_temporaries(root: &Path) -> Result<(), LockdownError> {
    for entry in fs::read_dir(root).map_err(|source| io_error("read directory", root, source))? {
        let entry = entry.map_err(|source| io_error("read directory", root, source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let stale = matches!(name, QUOTA_TEMP_FILE | TURN_BINDINGS_TEMP_FILE)
            || (name.starts_with(DATA_TEMP_PREFIX) && name.len() == DATA_TEMP_NAME_BYTES);
        if !stale {
            continue;
        }
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|source| io_error("inspect", &path, source))?;
        if kind.is_symlink() {
            return Err(LockdownError::SymlinkRefused { path });
        }
        if !kind.is_file() {
            return Err(LockdownError::InvalidLedger {
                path,
                reason: "internal temporary path is not a regular file".to_owned(),
            });
        }
        fs::remove_file(&path)
            .map_err(|source| io_error("remove stale temporary", &path, source))?;
    }
    Ok(())
}

fn directory_size(root: &Path) -> Result<u64, LockdownError> {
    let mut total = 0_u64;
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|source| io_error("read directory", &directory, source))?
        {
            let entry = entry.map_err(|source| io_error("read directory", &directory, source))?;
            let path = entry.path();
            let metadata = entry
                .file_type()
                .map_err(|source| io_error("inspect", &path, source))?;
            if metadata.is_symlink() {
                return Err(LockdownError::SymlinkRefused { path });
            }
            if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file()
                && !(directory == root
                    && matches!(
                        entry.file_name().to_str(),
                        Some(
                            QUOTA_FILE
                                | QUOTA_LOCK_FILE
                                | QUOTA_TEMP_FILE
                                | TURN_BINDINGS_FILE
                                | TURN_BINDINGS_TEMP_FILE
                        )
                    ))
                && !(directory == root
                    && entry.file_name().to_str().is_some_and(|name| {
                        name.starts_with(DATA_TEMP_PREFIX) && name.len() == DATA_TEMP_NAME_BYTES
                    }))
            {
                let length = entry
                    .metadata()
                    .map_err(|source| io_error("inspect", &path, source))?
                    .len();
                total = total.saturating_add(length);
            }
        }
    }
    Ok(total)
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> LockdownError {
    LockdownError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod toolshape_tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn toolshape_lockdown_quota_publication_failure_marks_the_already_applied_write() {
        let temp = tempfile::tempdir().expect("temporary root");
        let manager = LockdownManager::initialize(temp.path().join("ld")).expect("manager");
        let error = manager
            .write_with_post_apply("fixture", Path::new("written.txt"), b"landed bytes", || {
                fs::create_dir(manager.root.join(QUOTA_TEMP_FILE)).map_err(|source| {
                    io_error("inject quota publication failure", &manager.root, source)
                })
            })
            .expect_err("quota temporary directory prevents publication");
        let LockdownError::AppliedWrite(source) = &error else {
            panic!("write already landed: {error}");
        };
        assert_eq!(
            error.to_string(),
            source.to_string(),
            "original failure text stays unchanged"
        );
        assert_eq!(
            fs::read(
                manager
                    .provider_root("fixture")
                    .expect("provider root")
                    .join("written.txt")
            )
            .expect("landed file"),
            b"landed bytes"
        );
        assert!(matches!(source.as_ref(), LockdownError::Io { .. }));
        let refused = manager
            .write("fixture", Path::new("../outside"), b"denied")
            .expect_err("outside path refused");
        assert!(!matches!(refused, LockdownError::AppliedWrite(_)));
    }
}
