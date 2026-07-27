use crate::session_hub::SessionHubConfig;
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
    /// Aggregate frame cap on each connection's fair, attachment-keyed
    /// outbound queue (R12). One atomic ordinary-offer discipline enforces
    /// this cap together with the byte budget; attachment traffic waits for
    /// FIFO service while replies retain their priority path. The
    /// authoritative policy lives on `connection.rs`'s `OutboundLane`.
    pub outbound_queue_capacity: usize,
    /// Ordinary encoded-byte budget `B` per connection (R12 mechanism),
    /// including a frame until its socket write settles. The frame-count bound
    /// alone permits
    /// `outbound_queue_capacity × frame_limit` of resident payload, so bytes
    /// are charged before enqueue and credited once the write completes.
    /// A reply that ordinary traffic cannot admit falls through to one
    /// reserved `frame_limit + 4` floor; the final `ServerDraining` notice has
    /// a separate reservation of the same maximum encoded size (R17).
    ///
    /// Therefore the exact encoded-payload ceiling is
    /// `B + 2 × (frame_limit + 4)` per connection. At the defaults that is
    /// `16,777,216 + 2 × 8,388,612 = 33,554,440` bytes per connection and
    /// `64 × 33,554,440 = 2,147,484,160` bytes fleet-wide. Validation requires
    /// `B >= frame_limit + 4`, because the UDS length prefix is part of the
    /// ordinary charge and every legal maximum-size Event must fit an empty
    /// outbox. Tune these bounds together, not `frame_limit` alone.
    pub outbound_queued_bytes: usize,
    /// Simultaneously served connections. A same-UID peer accepted beyond this
    /// cap is answered with a fatal `overloaded` [`haider_rpc::ProtocolError`]
    /// and closed at once, so a faulty client cannot grow daemon tasks and
    /// queues without bound (report §2.5).
    pub max_connections: usize,
    /// How long an accepted peer may hold its connection slot without
    /// completing the `Hello` handshake. Silent peers are closed at this
    /// deadline so they cannot exhaust `max_connections` by saying nothing.
    pub handshake_timeout: Duration,
    /// Bounded completion window for the drain barrier (R17).
    ///
    /// This bounds the WHOLE barrier — connection completion, store flush,
    /// socket removal, and store close — not only the connection wait. A
    /// shutdown that overruns it, or that is forced during finalization,
    /// reports [`crate::ShutdownOutcome::Forced`], never `Graceful`.
    pub drain_timeout: Duration,
    /// Bounds for per-session actor commands, attachment admission, catch-up,
    /// and replay paging. Keeping them in the daemon's single config prevents
    /// production and injected runtimes from silently using different hub
    /// defaults.
    pub session_hub: SessionHubConfig,
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
            handshake_timeout: Duration::from_secs(10),
            drain_timeout: Duration::from_secs(5),
            session_hub: SessionHubConfig::default(),
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
        let encoded_frame_limit = self.frame_limit.checked_add(4).ok_or_else(|| {
            "frame limit plus the 4-byte UDS length prefix overflows usize".to_owned()
        })?;
        if self.outbound_queued_bytes < encoded_frame_limit {
            return Err(format!(
                "outbound queued-byte budget ({}) must be at least frame limit + the 4-byte UDS \
                 length prefix ({} + 4 = {} encoded bytes), so a legal maximum-size frame fits \
                 an empty outbox",
                self.outbound_queued_bytes, self.frame_limit, encoded_frame_limit
            ));
        }
        // The upper bound is the admission semaphore's own ceiling: a config
        // that cannot be represented must fail here, not panic at accept time.
        if self.max_connections == 0 || self.max_connections > Semaphore::MAX_PERMITS {
            return Err(format!(
                "maximum connections must be between 1 and {}",
                Semaphore::MAX_PERMITS
            ));
        }
        if self.handshake_timeout.is_zero() {
            return Err("handshake timeout must be greater than zero".into());
        }
        if self.drain_timeout.is_zero() {
            return Err("drain timeout must be greater than zero".into());
        }
        self.session_hub.validate()?;
        Ok(())
    }
}
