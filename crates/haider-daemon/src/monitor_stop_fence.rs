//! Monitor admission fence for explicit subagent stops (owner decision
//! 2026-09-25; Astra 13-final B1).
//!
//! A stop removes a subagent session's monitors while that session's runs
//! may still be executing. Without coordination a run can register a new
//! monitor after the stop listed the old ones, and that monitor later wakes
//! the stopped subagent into fresh work. The fence closes that window:
//!
//! * A [`MonitorStopFence`] owns immutable run and watch IDs. Registrations
//!   from those runs are refused and reports from those watches are discarded.
//!   Later independent work in the same session remains admissible.
//!   Registration checks the fence under
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
    runs: HashMap<(SessionId, RunId), usize>,
    monitors: HashMap<(SessionId, String), usize>,
    stopped_runs: HashSet<(SessionId, RunId)>,
    epochs: HashMap<(SessionId, String), u64>,
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

    /// Raises a fence for exactly the captured tasks and watches. Callers
    /// install it under monitor admission, using the durable stop claim.
    pub(crate) fn fence(
        &self,
        session: &SessionId,
        runs: &[RunId],
        monitors: &[String],
    ) -> MonitorStopFence {
        self.with(|state| {
            *state.sessions.entry(session.clone()).or_default() += 1;
            for run in runs {
                *state
                    .runs
                    .entry((session.clone(), run.clone()))
                    .or_default() += 1;
            }
            for monitor in monitors {
                let key = (session.clone(), monitor.clone());
                *state.monitors.entry(key.clone()).or_default() += 1;
                let epoch = state.epochs.entry(key).or_default();
                *epoch = epoch.saturating_add(1);
            }
        });
        MonitorStopFence {
            fences: self.clone(),
            session: session.clone(),
            runs: runs.to_vec(),
            monitors: monitors.to_vec(),
        }
    }

    /// Captured before a wake reads its source; a completed stop still changes
    /// this token after its temporary fence has dropped.
    pub(crate) fn epoch(&self, session: &SessionId, monitor: &str) -> u64 {
        self.with(|state| {
            state
                .epochs
                .get(&(session.clone(), monitor.to_owned()))
                .copied()
                .unwrap_or_default()
        })
    }

    /// Diagnostic only: never use a session-wide hold to decide ownership.
    #[cfg(test)]
    pub(crate) fn session_fenced(&self, session: &SessionId) -> bool {
        self.with(|state| state.sessions.contains_key(session))
    }

    /// True when a registration from `run` of `session` must be refused.
    pub(crate) fn registration_fenced(&self, session: &SessionId, run: &RunId) -> bool {
        self.with(|state| {
            state.runs.contains_key(&(session.clone(), run.clone()))
                || state.stopped_runs.contains(&(session.clone(), run.clone()))
        })
    }

    pub(crate) fn report_fenced(&self, session: &SessionId, monitor: &str) -> bool {
        self.with(|state| {
            state
                .monitors
                .contains_key(&(session.clone(), monitor.to_owned()))
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

    /// A deleted session can never submit another run. Reclaim stopped runs
    /// and watch epochs whose delivery ownership has ended with the session.
    pub(crate) fn forget_deleted_session(&self, session: &SessionId) {
        self.with(|state| {
            state.stopped_runs.retain(|(owner, _)| owner != session);
            state.epochs.retain(|(owner, _), _| owner != session);
        });
    }
}

/// One stop's hold on its captured tasks and watches; dropping it releases only those holds.
pub(crate) struct MonitorStopFence {
    fences: MonitorStopFences,
    session: SessionId,
    runs: Vec<RunId>,
    monitors: Vec<String>,
}

impl Drop for MonitorStopFence {
    fn drop(&mut self) {
        self.fences.with(|state| {
            for run in &self.runs {
                let key = (self.session.clone(), run.clone());
                if let Some(count) = state.runs.get_mut(&key) {
                    *count -= 1;
                    if *count == 0 {
                        state.runs.remove(&key);
                    }
                }
            }
            for monitor in &self.monitors {
                let key = (self.session.clone(), monitor.clone());
                if let Some(count) = state.monitors.get_mut(&key) {
                    *count -= 1;
                    if *count == 0 {
                        state.monitors.remove(&key);
                    }
                }
            }
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
