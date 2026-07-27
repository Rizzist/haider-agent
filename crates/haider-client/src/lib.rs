//! Shared client seam for the Haider daemon (report §6.2, R8/R9).
//!
//! Three cooperating pieces, deliberately below both binaries:
//!
//! - [`profile`] — the ONE profile resolver `haider` and `haiderd` share
//!   (store dir, profile id, runtime dir, endpoint path, release-owned
//!   defaults). `haider-daemon` delegates its endpoint derivation here so
//!   client and daemon can never disagree about the rendezvous path.
//! - [`client`] — the reusable UDS [`client::RpcClient`]: pending-request
//!   correlation, bounded writer/reader, R9 heartbeat, and reconnect
//!   primitives (typed disconnect reasons; the caller redials).
//! - [`spawn`] — bare-`haider` auto-spawn: connect first, spawn a detached
//!   sibling `haiderd` only on missing/refused endpoint, handshake-poll to a
//!   deadline, and report version/feature skew without ever killing a live
//!   daemon.
//!
//! This crate owns no TUI, daemon lifecycle, or persistence.

pub mod client;
pub mod profile;
pub mod spawn;

pub use client::{
    ClientConfig, ClientError, ConnectError, Connected, ConnectionState, DisconnectReason,
    PING_INTERVAL, PONG_DEADLINE, RpcClient, connect,
};
pub use profile::{
    DEFAULT_MAX_TOKENS, DEFAULT_PROVIDER, MODEL_ENV, PACKAGED_DEFAULT_MODEL, PROFILE_CONFIG_FILE,
    PROFILE_DIR_ENV, ProfileEnv, ProfileError, ResolvedProfile, effective_uid, endpoint_path_for,
    resolve_default_model_for, resolve_profile,
};
pub use spawn::{
    DAEMON_LOG_FILE, EnsureError, EnsureOptions, EnsuredDaemon, RACE_LOSER_EXIT_CODE,
    STARTUP_DEADLINE, ensure_daemon, required_live_features,
};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-client";
