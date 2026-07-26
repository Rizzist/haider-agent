use haider_rpc::DEFAULT_FRAME_LIMIT;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Complete, explicit configuration for one profile daemon.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub profile_id: String,
    pub store_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub frame_limit: usize,
    pub outbound_queue_capacity: usize,
    pub drain_timeout: Duration,
}

impl DaemonConfig {
    pub fn new(
        profile_id: impl Into<String>,
        store_dir: impl Into<PathBuf>,
        runtime_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            profile_id: profile_id.into(),
            store_dir: store_dir.into(),
            runtime_dir: runtime_dir.into(),
            frame_limit: DEFAULT_FRAME_LIMIT,
            outbound_queue_capacity: 32,
            drain_timeout: Duration::from_secs(5),
        }
    }

    /// Fixed-length, profile-derived rendezvous path.
    pub fn endpoint_path(&self) -> PathBuf {
        let digest = blake3::hash(self.profile_id.as_bytes()).to_hex();
        self.runtime_dir
            .join(format!("haider-{}.sock", &digest.as_str()[..32]))
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.profile_id.trim().is_empty() {
            return Err("profile id must not be empty".into());
        }
        if self.store_dir.as_os_str().is_empty() {
            return Err("store directory must not be empty".into());
        }
        if self.runtime_dir.as_os_str().is_empty() {
            return Err("runtime directory must not be empty".into());
        }
        if self.frame_limit == 0 || u32::try_from(self.frame_limit).is_err() {
            return Err("frame limit must be between 1 and u32::MAX".into());
        }
        if self.outbound_queue_capacity == 0 {
            return Err("outbound queue capacity must be greater than zero".into());
        }
        Ok(())
    }

    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }
}
