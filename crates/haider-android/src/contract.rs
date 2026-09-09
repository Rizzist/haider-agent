//! JNI-independent frozen C1/C3 parsing and status domain.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeStatus {
    Ok = 0,
    AlreadyRunning = 1,
    NotInitialized = 2,
    BadPaths = 3,
    BadPolicy = 4,
    VaultKeyInvalid = 5,
    StoreRecoveryFailed = 6,
    Internal = 7,
    ShutdownTimeout = 8,
    ShutdownForced = 9,
    BadArgument = 10,
}

impl NativeStatus {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::AlreadyRunning => "ALREADY_RUNNING",
            Self::NotInitialized => "NOT_INITIALIZED",
            Self::BadPaths => "BAD_PATHS",
            Self::BadPolicy => "BAD_POLICY",
            Self::VaultKeyInvalid => "VAULT_KEY_INVALID",
            Self::StoreRecoveryFailed => "STORE_RECOVERY_FAILED",
            Self::Internal => "INTERNAL",
            Self::ShutdownTimeout => "SHUTDOWN_TIMEOUT",
            Self::ShutdownForced => "SHUTDOWN_FORCED",
            Self::BadArgument => "BAD_ARGUMENT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub profile_id: String,
    pub store_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub tmp_dir: PathBuf,
}

impl Paths {
    pub fn validate(&self, root: &std::path::Path) -> Result<(), NativeStatus> {
        if self.profile_id != "android-default"
            || self.store_dir != root.join("haider/profiles/default")
            || self.runtime_dir != root.join("haider/runtime/android-default")
            || self.logs_dir != root.join("haider/logs")
            || self.workspace_dir != self.store_dir.join("workspace")
            || self.tmp_dir != self.runtime_dir.join("tmp")
        {
            return Err(NativeStatus::BadPaths);
        }
        for path in [
            &self.store_dir,
            &self.runtime_dir,
            &self.logs_dir,
            &self.workspace_dir,
            &self.tmp_dir,
        ] {
            if !path.is_absolute()
                || path.canonicalize().map_err(|_| NativeStatus::BadPaths)? != *path
            {
                return Err(NativeStatus::BadPaths);
            }
            let metadata = std::fs::symlink_metadata(path).map_err(|_| NativeStatus::BadPaths)?;
            if !metadata.is_dir() {
                return Err(NativeStatus::BadPaths);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.mode() & 0o777 != 0o700
                {
                    return Err(NativeStatus::BadPaths);
                }
            }
        }
        for name in ["h.sock", "mobile.sock"] {
            haider_platform::Endpoint::from_address(self.runtime_dir.join(name))
                .validate_for_bind(&self.runtime_dir)
                .map_err(|_| NativeStatus::BadPaths)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub policy_version: u32,
    pub name: String,
    pub default_model: String,
    pub store_synchronous: haider_protocol::runtime::StoreSynchronous,
}

impl Policy {
    pub fn parse(text: &str) -> Result<Self, NativeStatus> {
        let policy: Self = serde_json::from_str(text).map_err(|_| NativeStatus::BadPolicy)?;
        if policy.policy_version != 1
            || policy.name != "android-standalone"
            || policy.default_model.trim().is_empty()
        {
            return Err(NativeStatus::BadPolicy);
        }
        Ok(policy)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub jni_version: u32,
    pub phase: &'static str,
    pub daemon_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_since_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl Observation {
    pub const fn phase(phase: &'static str, generation: u64) -> Self {
        Self {
            jni_version: 1,
            phase,
            daemon_generation: generation,
            ready_since_unix_ms: None,
            endpoint_path: None,
            error_code: None,
            retryable: None,
        }
    }
    pub fn failed(status: NativeStatus, generation: u64) -> Self {
        Self {
            error_code: Some(status.code()),
            retryable: Some(false),
            ..Self::phase("Failed", generation)
        }
    }
}

pub fn shutdown_budget(deadline_ms: i64) -> Result<std::time::Duration, NativeStatus> {
    if deadline_ms < 0 {
        return Err(NativeStatus::BadArgument);
    }
    Ok(std::time::Duration::from_millis(if deadline_ms == 0 {
        7000
    } else {
        deadline_ms as u64
    }))
}
