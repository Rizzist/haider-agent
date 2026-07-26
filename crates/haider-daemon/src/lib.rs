//! Lifecycle owner for the long-running Haider profile daemon.
//!
//! This crate deliberately stops at handshake, ping, recovery, and shutdown.
//! Session routing, attachment replay, and menu arbitration are W3b2 seams
//! (their stubs live in `connection.rs` and answer `draining` / `not_found`).
//!
//! Laws enforced here (d1 report R1/R2/R3/R16/R17):
//!
//! - **Lock first, release last** — the store's profile lifetime lock is the
//!   only singleton authority. It is acquired before any socket cleanup or
//!   store open and released last, by closing the store after every other
//!   shutdown step (`runtime.rs`).
//! - **Probe, then lstat-verified unlink** — a stale rendezvous socket is
//!   removed only after a connect probe refuses AND the node is verified as a
//!   same-user socket in the owned runtime directory (`endpoint.rs`).
//! - **Device+inode identity** — the daemon records its bound socket's
//!   device+inode and cleanup removes only that exact node, so an old daemon
//!   can never delete a successor's socket (`endpoint.rs`).
//! - **Reconcile before ready** — the daemon generation is durably bumped and
//!   every dispatched-without-terminal effect is reconciled (via
//!   `haider_core::reconcile_dispatched_effects`) before the listener binds
//!   or `Ready` is advertised (`runtime.rs`).
//! - **Honest drain** — first shutdown request drains: stop accepting, notify
//!   every connection with `ServerDraining`, bounded completion, flush,
//!   remove the exact owned socket, release the lock last. A second request
//!   forces termination; recovery is the next generation's job (`runtime.rs`).
//!
//! The phase machine itself lives in `lifecycle.rs`; its legal transitions are
//! documented on [`DaemonState`] and enforced by the state publisher.

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
