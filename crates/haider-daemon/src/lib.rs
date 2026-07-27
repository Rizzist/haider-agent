//! Lifecycle owner for the long-running Haider profile daemon.
//!
//! In addition to singleton/recovery/shutdown ownership, this crate owns the
//! W3b2 per-session actor hub: list/read/attach/detach, snapshot-free replay
//! and its live barrier, bounded fair delivery, and durable menu arbitration.
//!
//! Laws enforced here (d1 report R1/R2/R3/R16/R17):
//!
//! - **Lock first, release last** — the store's profile lifetime lock is the
//!   only singleton authority. It is acquired before any socket cleanup or
//!   store open and released last, by closing the store after every other
//!   shutdown step (`runtime.rs`).
//! - **Probe, then verified unlink — descriptor-relative** — a stale
//!   rendezvous socket is removed only after a connect probe refuses AND the
//!   node is verified as a same-user socket, with every filesystem step issued
//!   against the runtime directory's own descriptor so no path swap can
//!   redirect it (`endpoint.rs`).
//! - **Device+inode identity, verified where it is removed** — the socket is
//!   created under an unpredictable name and renamed into place, so the
//!   recorded device+inode cannot have been pre-staged by anyone else; cleanup
//!   claims the public name back under a fresh unpredictable name and verifies
//!   identity there, so an old daemon does not delete a successor's socket. The
//!   residual — a same-UID process that WATCHES the directory can still learn a
//!   staging name as it appears — is scoped precisely in `endpoint.rs`.
//! - **Reconcile before ready** — the daemon generation is durably bumped and
//!   every dispatched-without-terminal effect is reconciled (via
//!   `haider_core::reconcile_dispatched_effects`) before the listener binds
//!   or `Ready` is advertised (`runtime.rs`).
//! - **Honest drain** — first shutdown request drains: stop accepting, notify
//!   every connection with `ServerDraining`, bounded completion, flush,
//!   remove the exact owned socket, release the lock last. ONE deadline covers
//!   the whole barrier, finalization included, and a second request forces
//!   termination at any point in it; an overrun, a force, or a connection that
//!   never received its notice is reported as `Forced`, never `Graceful`.
//!   Recovery is the next generation's job (`runtime.rs`).
//! - **Snapshot-free attachment** — one actor serializes each live session.
//!   Receiver registration and head capture are one actor turn, appends are
//!   persisted before publication, and replay tasks are cancellable/owned.
//! - **Store-backed lag and menu CAS** — overflow has two responses: internal
//!   catch-up overflow re-registers and resumes from the store without
//!   detaching (the client just sees a repeated `AttachCaughtUp`), while
//!   sink-side refusal or lag-under-stall detaches with a system-lane
//!   `Lagged` and the client reattaches after its applied
//!   `RawEnvelope.seq`. Menu answers
//!   append through one durable compare-and-set and every attachment learns
//!   the winner from the event stream.
//!
//! The phase machine itself lives in `lifecycle.rs`; its legal transitions are
//! documented on [`DaemonState`] and enforced by the state publisher.

mod config;
mod connection;
mod endpoint;
mod error;
mod lifecycle;
mod runtime;
mod session_hub;

pub use config::DaemonConfig;
pub use error::{DaemonError, IncumbentDiagnostics};
pub use lifecycle::{DaemonState, Readiness, ShutdownDisposition, ShutdownHandle, ShutdownOutcome};
pub use runtime::{DaemonTask, run_with_signals, spawn};
pub use session_hub::{
    FrameSendError, FrameSink, HubConnection, HubObservation, SendAdmission, SessionHub,
    SessionHubConfig, SessionHubError, SessionHubObserver,
};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-daemon";
