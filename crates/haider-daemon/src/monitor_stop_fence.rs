//! Monitor admission fence for explicit subagent stops (owner decision
//! 2026-09-25; Astra 13-final B1).
//!
//! A stop removes a subagent session's monitors while that session's runs
//! may still be executing. Without coordination a run can register a new
//! monitor after the stop listed the old ones, and that monitor later wakes
//! the stopped subagent into fresh work. The fence closes that window:
//!
//! * While any stop holds a [`MonitorStopFence`] for a session, monitor
//!   registrations from that session's runs are refused and monitor wakes
//!   into that session are discarded. Registration checks the fence under
//!   the monitor service's mutation lock, which every listing also takes, so
//!   a registration either commits before the stop's listing (and is removed
//!   by it) or observes the fence.
//! * A cancelling run targeted by a stop is remembered as stopped for as long
//!   as it is not observed terminal, so a slow-to-settle run cannot register
//!   a monitor after the stop's fence is released.

use haider_protocol::ids::{RunId, SessionId};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Default)]
struct FenceState {
    sessions: HashMap<SessionId, usize>,
    stopped_runs: HashSet<(SessionId, RunId)>,
    epochs: HashMap<SessionId, u64>,
}

/// Shared fence registry owned by the monitor service.
#[derive(Clone, Default)]
pub(crate) struct MonitorStopFences {
    state: Arc<Mutex<FenceState>>,
}

impl MonitorStopFences {
    fn with<T>(&self, apply: impl FnOnce(&mut FenceState) -> T) -> T {
        apply(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Raises the fence for one session until the returned guard drops.
    pub(crate) fn fence(&self, session: &SessionId) -> MonitorStopFence {
        self.with(|state| {
            *state.sessions.entry(session.clone()).or_default() += 1;
            let epoch = state.epochs.entry(session.clone()).or_default();
            *epoch = epoch.saturating_add(1);
        });
        MonitorStopFence {
            fences: self.clone(),
            session: session.clone(),
        }
    }

    /// Captured before a wake reads its source; a completed stop still changes
    /// this token after its temporary fence has dropped.
    pub(crate) fn epoch(&self, session: &SessionId) -> u64 {
        self.with(|state| state.epochs.get(session).copied().unwrap_or_default())
    }

    /// True while a stop of `session` is in progress.
    pub(crate) fn session_fenced(&self, session: &SessionId) -> bool {
        self.with(|state| state.sessions.contains_key(session))
    }

    /// True when a registration from `run` of `session` must be refused.
    pub(crate) fn registration_fenced(&self, session: &SessionId, run: &RunId) -> bool {
        self.with(|state| {
            state.sessions.contains_key(session)
                || state.stopped_runs.contains(&(session.clone(), run.clone()))
        })
    }

    /// Remembers a cancelling run targeted by a stop.
    pub(crate) fn mark_run_stopped(&self, session: &SessionId, run: &RunId) {
        self.with(|state| {
            state.stopped_runs.insert((session.clone(), run.clone()));
        });
    }

    /// Forgets a stopped run once it is observed terminal.
    pub(crate) fn forget_stopped_run(&self, session: &SessionId, run: &RunId) {
        self.with(|state| {
            state.stopped_runs.remove(&(session.clone(), run.clone()));
        });
    }

    /// A deleted session can never submit another run. Reclaim any stopped
    /// run entries that never reached a terminal observation.
    pub(crate) fn forget_deleted_session(&self, session: &SessionId) {
        self.with(|state| state.stopped_runs.retain(|(owner, _)| owner != session));
    }
}

/// One stop's hold on a session's fence; dropping it lowers the fence.
pub(crate) struct MonitorStopFence {
    fences: MonitorStopFences,
    session: SessionId,
}

impl Drop for MonitorStopFence {
    fn drop(&mut self) {
        self.fences.with(|state| {
            if let Some(count) = state.sessions.get_mut(&self.session) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.sessions.remove(&self.session);
                }
            }
        });
    }
}

#[cfg(test)]
#[path = "monitor_stop_fence_tests.rs"]
mod monitor_stop_fence_tests;
