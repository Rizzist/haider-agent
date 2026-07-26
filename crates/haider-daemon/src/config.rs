use haider_rpc::DEFAULT_FRAME_LIMIT;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Complete, explicit configuration for one profile daemon.
///
/// All fields are plain data; validation happens once at daemon start and
/// violations surface as `DaemonError::InvalidConfig`.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Profile identity; the singleton scope. One daemon per profile.
    pub profile_id: String,
    /// Root of the profile store — the lifetime lock and SQLite journal.
    pub store_dir: PathBuf,
    /// Per-user runtime directory (forced to `0700`) holding the socket (R2).
    pub runtime_dir: PathBuf,
    /// Maximum inbound frame size; also advertised in `Welcome.frame_limit`,
    /// so it must fit `u32`.
    pub frame_limit: usize,
    /// Depth of each connection's bounded outbound queue (R12 mechanism);
    /// an unwritable full queue is a connection-fatal error, never a stall.
    pub outbound_queue_capacity: usize,
    /// Ceiling on encoded bytes a connection may hold queued-but-unwritten
    /// (R12 mechanism). The frame-count bound alone permits
    /// `outbound_queue_capacity × frame_limit` of resident payload, so bytes
    /// are charged before enqueue and credited once the write completes;
    /// exceeding the budget is the same connection-fatal error a full queue is.
    /// Values below `frame_limit` cannot carry a maximum-size frame at all —
    /// the default deliberately leaves room for one in flight plus one queued.
    /// The final `ServerDraining` frame never spends this budget (R17).
    pub outbound_queued_bytes: usize,
    /// Simultaneously served connections. A same-UID peer accepted beyond this
    /// cap is answered with a fatal `overloaded` [`haider_rpc::ProtocolError`]
    /// and closed at once, so a faulty client cannot grow daemon tasks and
    /// queues without bound (report §2.5).
    pub max_connections: usize,
    /// Bounded completion window for the drain barrier (R17).
    pub drain_timeout: Duration,
}

impl DaemonConfig {
    /// Builds a config with production defaults for the tuning knobs.
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
            outbound_queued_bytes: DEFAULT_FRAME_LIMIT.saturating_mul(2),
            max_connections: 64,
            drain_timeout: Duration::from_secs(5),
        }
    }

    /// Fixed-length, profile-derived rendezvous path (R2).
    ///
    /// Hashing the profile id keeps the socket name a constant 32 hex chars
    /// regardless of profile-id length or charset, which protects the tight
    /// OS limit on Unix socket path length (`sun_path`, ~104 bytes on macOS).
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
        if self.outbound_queued_bytes == 0 {
            return Err("outbound queued-byte budget must be greater than zero".into());
        }
        // The upper bound is the admission semaphore's own ceiling: a config
        // that cannot be represented must fail here, not panic at accept time.
        if self.max_connections == 0 || self.max_connections > Semaphore::MAX_PERMITS {
            return Err(format!(
                "maximum connections must be between 1 and {}",
                Semaphore::MAX_PERMITS
            ));
        }
        Ok(())
    }
}
