use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use haider_accounts::Vault;
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{CredentialAlias, SessionId};
use haider_rpc::{PinnedHostKeyWire, SshProfileUpdateWire, SshProfileWire, SshScopeWire};
use serde::{Deserialize, Serialize};

const PROFILE_ALIAS_PREFIX: &str = "haider.ssh.profile.";
const SECRET_ALIAS_PREFIX: &str = "haider.ssh.secret.";
const SCOPE_ALIAS_PREFIX: &str = "haider.ssh.scope.";
const PROFILE_FORMAT_VERSION: u32 = 1;
const SCOPE_FORMAT_VERSION: u32 = 1;
const DESCRIPTION_MAX_CHARS: usize = 1_024;
const HOST_MAX_BYTES: usize = 255;
const USER_MAX_BYTES: usize = 255;
const PATH_MAX_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SshProfile {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) ssh: SshTarget,
    pub(crate) last_used_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SshTarget {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) auth: SshAuth,
    pub(crate) default_cwd: Option<String>,
    pub(crate) host_key: Option<PinnedHostKey>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SshAuth {
    KeyFile {
        path: String,
        passphrase_vault_ref: Option<String>,
    },
    KeyMaterial {
        vault_ref: String,
    },
    Agent,
    Password {
        vault_ref: String,
    },
}

impl fmt::Debug for SshAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyFile {
                passphrase_vault_ref,
                ..
            } => formatter
                .debug_struct("KeyFile")
                .field("path", &"<redacted>")
                .field("has_passphrase", &passphrase_vault_ref.is_some())
                .finish(),
            Self::KeyMaterial { .. } => formatter.write_str("KeyMaterial([REDACTED])"),
            Self::Agent => formatter.write_str("Agent"),
            Self::Password { .. } => formatter.write_str("Password([REDACTED])"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PinnedHostKey {
    pub(crate) algorithm: String,
    pub(crate) fingerprint: String,
    pub(crate) pinned_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum SshScope {
    #[default]
    All,
    Allow(BTreeSet<String>),
    None,
}

impl SshScope {
    pub(crate) fn from_wire(scope: SshScopeWire) -> Result<Self, SshError> {
        match scope {
            SshScopeWire::All => Ok(Self::All),
            SshScopeWire::None => Ok(Self::None),
            SshScopeWire::Allow { names } => {
                let mut allowed = BTreeSet::new();
                for name in names {
                    validate_name(&name)?;
                    allowed.insert(name);
                }
                Ok(Self::Allow(allowed))
            }
        }
    }

    pub(crate) fn to_wire(&self) -> SshScopeWire {
        match self {
            Self::All => SshScopeWire::All,
            Self::None => SshScopeWire::None,
            Self::Allow(names) => SshScopeWire::Allow {
                names: names.iter().cloned().collect(),
            },
        }
    }

    pub(crate) fn allows(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Allow(names) => names.contains(name),
            Self::None => false,
        }
    }
}

pub(crate) fn enforce_scope(
    scope: &SshScope,
    session_id: &SessionId,
    name: &str,
) -> Result<(), SshError> {
    if scope.allows(name) {
        Ok(())
    } else {
        Err(SshError::SshProfileOutOfScope {
            session_id: session_id.clone(),
            name: name.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SshError {
    SshProfileNotFound {
        name: String,
    },
    SshProfileExists {
        name: String,
    },
    SshProfileInvalidName {
        name: String,
    },
    SshProfileInvalid {
        field: &'static str,
        message: String,
    },
    SshProfileOutOfScope {
        session_id: SessionId,
        name: String,
    },
    SshHostKeyChanged {
        expected: String,
        actual: String,
    },
    SshAuthenticationFailed {
        name: String,
    },
    SshAgentUnavailable,
    SshKeyInvalid {
        name: String,
    },
    SshConnection {
        message: String,
    },
    SshChannelClosed {
        name: String,
    },
    SshChannelQuota {
        name: String,
        limit: usize,
    },
    Vault {
        message: String,
    },
    StoreCorrupt {
        name: String,
    },
}

impl fmt::Display for SshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SshProfileNotFound { name } => {
                write!(formatter, "SSH profile `{name}` was not found")
            }
            Self::SshProfileExists { name } => {
                write!(formatter, "SSH profile `{name}` already exists")
            }
            Self::SshProfileInvalidName { name } => write!(
                formatter,
                "SSH profile name `{name}` must match [a-z0-9._-]{{1,32}}"
            ),
            Self::SshProfileInvalid { field, message } => {
                write!(formatter, "invalid SSH profile {field}: {message}")
            }
            Self::SshProfileOutOfScope { session_id, name } => write!(
                formatter,
                "SSH profile `{name}` is outside session `{session_id}` scope"
            ),
            Self::SshHostKeyChanged { expected, actual } => write!(
                formatter,
                "SSH host key changed (expected {expected}, actual {actual})"
            ),
            Self::SshAuthenticationFailed { name } => {
                write!(formatter, "SSH authentication failed for profile `{name}`")
            }
            Self::SshAgentUnavailable => {
                formatter.write_str("SSH agent is unavailable on this platform or device")
            }
            Self::SshKeyInvalid { name } => {
                write!(
                    formatter,
                    "SSH key for profile `{name}` is invalid or cannot be decrypted"
                )
            }
            Self::SshConnection { message } => {
                write!(formatter, "SSH connection failed: {message}")
            }
            Self::SshChannelClosed { name } => {
                write!(formatter, "SSH shell for profile `{name}` was closed")
            }
            Self::SshChannelQuota { name, limit } => write!(
                formatter,
                "SSH profile `{name}` already has the maximum {limit} concurrent channels"
            ),
            Self::Vault { message } => write!(formatter, "SSH profile vault failure: {message}"),
            Self::StoreCorrupt { name } => {
                write!(formatter, "SSH profile secret record `{name}` is corrupt")
            }
        }
    }
}

impl std::error::Error for SshError {}

impl SshError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::SshProfileNotFound { .. } => "ssh_profile_not_found",
            Self::SshProfileExists { .. } => "ssh_profile_exists",
            Self::SshProfileInvalidName { .. } => "ssh_profile_invalid_name",
            Self::SshProfileInvalid { .. } => "ssh_profile_invalid",
            Self::SshProfileOutOfScope { .. } => "ssh_profile_out_of_scope",
            Self::SshHostKeyChanged { .. } => "ssh_host_key_changed",
            Self::SshAuthenticationFailed { .. } => "ssh_authentication_failed",
            Self::SshAgentUnavailable => "ssh_agent_unavailable",
            Self::SshKeyInvalid { .. } => "ssh_key_invalid",
            Self::SshConnection { .. } => "ssh_connection_failed",
            Self::SshChannelClosed { .. } => "ssh_channel_closed",
            Self::SshChannelQuota { .. } => "ssh_channel_quota",
            Self::Vault { .. } => "ssh_vault_error",
            Self::StoreCorrupt { .. } => "ssh_profile_store_corrupt",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredProfile {
    version: u32,
    profile: SshProfile,
}

#[derive(Serialize, Deserialize)]
struct StoredScope {
    version: u32,
    scope: SshScopeWire,
}

#[derive(Clone)]
pub(crate) struct SshProfileStore {
    vault: Arc<dyn Vault>,
}

impl SshProfileStore {
    pub(crate) fn new(vault: Arc<dyn Vault>) -> Self {
        Self { vault }
    }

    pub(crate) fn list(&self) -> Result<Vec<SshProfile>, SshError> {
        let mut profiles = Vec::new();
        for alias in self.vault.list().map_err(vault_error)? {
            let Some(name) = alias.as_str().strip_prefix(PROFILE_ALIAS_PREFIX) else {
                continue;
            };
            profiles.push(self.read_alias(name, &alias)?);
        }
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(profiles)
    }

    /// Loads one durable session scope from the profile-scoped secret vault.
    /// Legacy sessions and sessions never narrowed default to `All`.
    pub(crate) fn session_scope(&self, session_id: &SessionId) -> Result<SshScope, SshError> {
        let alias = scope_alias(session_id);
        match self.vault.resolve(&alias) {
            Ok(secret) => {
                let stored: StoredScope =
                    serde_json::from_slice(secret.expose_secret()).map_err(|_| {
                        SshError::StoreCorrupt {
                            name: format!("scope:{}", session_id.as_str()),
                        }
                    })?;
                if stored.version != SCOPE_FORMAT_VERSION {
                    return Err(SshError::StoreCorrupt {
                        name: format!("scope:{}", session_id.as_str()),
                    });
                }
                SshScope::from_wire(stored.scope)
            }
            Err(error) if error.code == ErrorCode::CredentialMissing => Ok(SshScope::All),
            Err(error) => Err(vault_error(error)),
        }
    }

    /// Persists scope before publishing it to the in-memory hot cache, so a
    /// daemon restart cannot widen an existing session back to `All`.
    pub(crate) fn set_session_scope(
        &self,
        session_id: &SessionId,
        scope: &SshScope,
    ) -> Result<(), SshError> {
        let bytes = serde_json::to_vec(&StoredScope {
            version: SCOPE_FORMAT_VERSION,
            scope: scope.to_wire(),
        })
        .map_err(|_| SshError::StoreCorrupt {
            name: format!("scope:{}", session_id.as_str()),
        })?;
        self.vault
            .put(&scope_alias(session_id), &bytes)
            .map_err(vault_error)
    }

    pub(crate) fn get(&self, name: &str) -> Result<SshProfile, SshError> {
        validate_name(name)?;
        let alias = profile_alias(name);
        match self.vault.resolve(&alias) {
            Ok(secret) => decode_profile(name, secret.expose_secret()),
            Err(error) if error.code == ErrorCode::CredentialMissing => {
                Err(SshError::SshProfileNotFound { name: name.into() })
            }
            Err(error) => Err(vault_error(error)),
        }
    }

    pub(crate) fn add(&self, profile: SshProfile) -> Result<SshProfile, SshError> {
        validate_profile(&profile)?;
        match self.get(&profile.name) {
            Ok(_) => {
                return Err(SshError::SshProfileExists {
                    name: profile.name.clone(),
                });
            }
            Err(SshError::SshProfileNotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        self.write(&profile)?;
        Ok(profile)
    }

    pub(crate) fn replace(&self, profile: SshProfile) -> Result<SshProfile, SshError> {
        validate_profile(&profile)?;
        let previous = self.get(&profile.name)?;
        self.write(&profile)?;
        self.retire_replaced_secrets(&previous.ssh.auth, &profile.ssh.auth);
        Ok(profile)
    }

    pub(crate) fn pin_host_key(
        &self,
        name: &str,
        observed: PinnedHostKey,
    ) -> Result<bool, SshError> {
        let mut profile = self.get(name)?;
        match &profile.ssh.host_key {
            Some(expected) if expected.fingerprint != observed.fingerprint => {
                return Err(SshError::SshHostKeyChanged {
                    expected: expected.fingerprint.clone(),
                    actual: observed.fingerprint,
                });
            }
            Some(_) => return Ok(false),
            None => {}
        }
        profile.ssh.host_key = Some(observed);
        self.replace(profile)?;
        Ok(true)
    }

    pub(crate) fn mark_used(&self, name: &str, used_at_ms: u64) -> Result<(), SshError> {
        let mut profile = self.get(name)?;
        profile.last_used_ms = Some(used_at_ms);
        self.replace(profile).map(|_| ())
    }

    pub(crate) fn update_non_secret(
        &self,
        name: &str,
        changes: SshProfileUpdateWire,
        auth: Option<SshAuth>,
    ) -> Result<SshProfile, SshError> {
        let mut profile = self.get(name)?;
        if let Some(description) = changes.description {
            profile.description = description;
        }
        if let Some(host) = changes.host {
            if host != profile.ssh.host {
                profile.ssh.host = host;
                profile.ssh.host_key = None;
            }
        }
        if let Some(port) = changes.port {
            if port != profile.ssh.port {
                profile.ssh.host_key = None;
            }
            profile.ssh.port = port;
        }
        if let Some(user) = changes.user {
            profile.ssh.user = user;
        }
        if let Some(default_cwd) = changes.default_cwd {
            profile.ssh.default_cwd = default_cwd;
        }
        if let Some(auth) = auth {
            profile.ssh.auth = auth;
        }
        self.replace(profile)
    }

    pub(crate) fn remove(&self, name: &str) -> Result<(), SshError> {
        let profile = self.get(name)?;
        self.vault
            .delete(&profile_alias(name))
            .map_err(vault_error)?;
        for vault_ref in auth_secret_refs(&profile.ssh.auth) {
            self.vault
                .delete(&CredentialAlias::new(vault_ref))
                .map_err(vault_error)?;
        }
        Ok(())
    }

    pub(crate) fn put_auth_secret(&self, name: &str, secret: &[u8]) -> Result<String, SshError> {
        validate_name(name)?;
        let mut random = [0_u8; 10];
        getrandom::fill(&mut random).map_err(|error| SshError::Vault {
            message: format!("cannot allocate credential reference: {error}"),
        })?;
        let alias = secret_alias(name, &hex::encode(random));
        self.vault.put(&alias, secret).map_err(vault_error)?;
        Ok(alias.as_str().to_owned())
    }

    pub(crate) fn discard_auth_secret(&self, auth: &SshAuth) {
        for vault_ref in auth_secret_refs(auth) {
            let _ = self.vault.delete(&CredentialAlias::new(vault_ref));
        }
    }

    pub(crate) fn resolve_auth_secret(
        &self,
        name: &str,
        vault_ref: &str,
    ) -> Result<haider_accounts::SecretHandle, SshError> {
        validate_name(name)?;
        let expected_prefix = format!("{SECRET_ALIAS_PREFIX}{name}.");
        if !vault_ref.starts_with(&expected_prefix) {
            return Err(SshError::StoreCorrupt {
                name: "invalid-secret-reference".into(),
            });
        }
        self.vault
            .resolve(&CredentialAlias::new(vault_ref))
            .map_err(vault_error)
    }

    fn read_alias(&self, name: &str, alias: &CredentialAlias) -> Result<SshProfile, SshError> {
        let secret = self.vault.resolve(alias).map_err(vault_error)?;
        decode_profile(name, secret.expose_secret())
    }

    fn write(&self, profile: &SshProfile) -> Result<(), SshError> {
        let bytes = serde_json::to_vec(&StoredProfile {
            version: PROFILE_FORMAT_VERSION,
            profile: profile.clone(),
        })
        .map_err(|_| SshError::StoreCorrupt {
            name: profile.name.clone(),
        })?;
        self.vault
            .put(&profile_alias(&profile.name), &bytes)
            .map_err(vault_error)
    }

    fn retire_replaced_secrets(&self, previous: &SshAuth, current: &SshAuth) {
        let current = auth_secret_refs(current);
        for vault_ref in auth_secret_refs(previous) {
            if !current.contains(&vault_ref) {
                let _ = self.vault.delete(&CredentialAlias::new(vault_ref));
            }
        }
    }
}

impl SshProfile {
    pub(crate) fn public(&self) -> SshProfileWire {
        self.public_with_scope(true)
    }

    pub(crate) fn public_with_scope(&self, in_scope: bool) -> SshProfileWire {
        SshProfileWire {
            name: self.name.clone(),
            description: self.description.clone(),
            host: self.ssh.host.clone(),
            port: self.ssh.port,
            user: self.ssh.user.clone(),
            default_cwd: self.ssh.default_cwd.clone(),
            host_key: self.ssh.host_key.as_ref().map(PinnedHostKey::public),
            last_used_ms: self.last_used_ms,
            multiplexing: true,
            in_scope,
        }
    }
}

impl PinnedHostKey {
    fn public(&self) -> PinnedHostKeyWire {
        PinnedHostKeyWire {
            algorithm: self.algorithm.clone(),
            fingerprint: self.fingerprint.clone(),
            pinned_at_ms: self.pinned_at_ms,
        }
    }
}

pub(crate) fn validate_name(name: &str) -> Result<(), SshError> {
    if name.is_empty()
        || name.len() > 32
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        return Err(SshError::SshProfileInvalidName { name: name.into() });
    }
    Ok(())
}

fn validate_profile(profile: &SshProfile) -> Result<(), SshError> {
    validate_name(&profile.name)?;
    validate_bounded("host", &profile.ssh.host, HOST_MAX_BYTES)?;
    validate_bounded("user", &profile.ssh.user, USER_MAX_BYTES)?;
    if profile.ssh.port == 0 {
        return Err(SshError::SshProfileInvalid {
            field: "port",
            message: "must be between 1 and 65535".into(),
        });
    }
    if profile
        .description
        .as_ref()
        .is_some_and(|description| description.chars().count() > DESCRIPTION_MAX_CHARS)
    {
        return Err(SshError::SshProfileInvalid {
            field: "description",
            message: format!("must be at most {DESCRIPTION_MAX_CHARS} characters"),
        });
    }
    if let Some(cwd) = &profile.ssh.default_cwd {
        validate_bounded("default_cwd", cwd, PATH_MAX_BYTES)?;
    }
    if let SshAuth::KeyFile { path, .. } = &profile.ssh.auth {
        validate_bounded("key path", path, PATH_MAX_BYTES)?;
    }
    Ok(())
}

fn validate_bounded(field: &'static str, value: &str, max: usize) -> Result<(), SshError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(SshError::SshProfileInvalid {
            field,
            message: format!("must be non-empty, control-free, and at most {max} bytes"),
        });
    }
    Ok(())
}

fn profile_alias(name: &str) -> CredentialAlias {
    CredentialAlias::new(format!("{PROFILE_ALIAS_PREFIX}{name}"))
}

fn secret_alias(name: &str, nonce: &str) -> CredentialAlias {
    CredentialAlias::new(format!("{SECRET_ALIAS_PREFIX}{name}.{nonce}"))
}

fn scope_alias(session_id: &SessionId) -> CredentialAlias {
    CredentialAlias::new(format!("{SCOPE_ALIAS_PREFIX}{}", session_id.as_str()))
}

fn auth_secret_refs(auth: &SshAuth) -> Vec<&str> {
    match auth {
        SshAuth::KeyFile {
            passphrase_vault_ref,
            ..
        } => passphrase_vault_ref.iter().map(String::as_str).collect(),
        SshAuth::KeyMaterial { vault_ref } | SshAuth::Password { vault_ref } => {
            vec![vault_ref]
        }
        SshAuth::Agent => Vec::new(),
    }
}

fn decode_profile(name: &str, bytes: &[u8]) -> Result<SshProfile, SshError> {
    let stored: StoredProfile =
        serde_json::from_slice(bytes).map_err(|_| SshError::StoreCorrupt { name: name.into() })?;
    if stored.version != PROFILE_FORMAT_VERSION || stored.profile.name != name {
        return Err(SshError::StoreCorrupt { name: name.into() });
    }
    validate_profile(&stored.profile)?;
    Ok(stored.profile)
}

fn vault_error(error: haider_protocol::error::HaiderError) -> SshError {
    SshError::Vault {
        message: error.message,
    }
}
