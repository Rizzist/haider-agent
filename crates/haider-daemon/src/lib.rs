//! Lifecycle owner for the long-running Haider profile daemon.
//!
//! This crate deliberately stops at handshake, ping, recovery, and shutdown.
//! Session routing, attachment replay, and menu arbitration are W3b2 seams.

mod config;
mod connection;
mod endpoint;
mod error;
mod lifecycle;
mod runtime;

pub use config::DaemonConfig;
pub use error::{DaemonError, IncumbentDiagnostics};
pub use lifecycle::{DaemonState, Readiness, ShutdownDisposition, ShutdownHandle, ShutdownOutcome};
pub use runtime::{DaemonTask, run_with_signals, spawn};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-daemon";
