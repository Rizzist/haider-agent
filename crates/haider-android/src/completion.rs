//! Completion and the JNI observation share one terminal publication.
use crate::contract::{NativeStatus, Observation};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub(crate) struct CompletionReceipt {
    state: Mutex<CompletionState>,
    completed: Condvar,
}

struct CompletionState {
    status: Option<NativeStatus>,
    observation: Observation,
}

impl Default for CompletionReceipt {
    fn default() -> Self {
        Self::new(Observation::phase("Stopped", 0))
    }
}

impl CompletionReceipt {
    pub(crate) fn new(observation: Observation) -> Self {
        Self {
            state: Mutex::new(CompletionState {
                status: None,
                observation,
            }),
            completed: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CompletionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn get(&self) -> Option<NativeStatus> {
        self.lock().status
    }

    /// The refresh reads only in-memory diagnostics. No I/O under this lock.
    pub(crate) fn observe(&self, refresh: impl FnOnce(&mut Observation)) -> Observation {
        let mut state = self.lock();
        if state.status.is_none() && state.observation.error_code.is_none() {
            refresh(&mut state.observation);
        }
        state.observation.clone()
    }

    pub(crate) fn fail(&self, status: NativeStatus) {
        let mut state = self.lock();
        if state.status.is_none() {
            state.observation = Observation::failed(status, state.observation.daemon_generation);
        }
    }

    /// The owner calls this only after runtime and blocking-reader teardown.
    /// `publish` updates the process's retained snapshot before waiters can
    /// release LIVE. It must not do I/O or call back into this receipt.
    pub(crate) fn finish(&self, result: NativeStatus, publish: impl FnOnce(&Observation)) {
        let mut state = self.lock();
        if state.status.is_none() {
            let generation = state.observation.daemon_generation;
            let observation = if matches!(result, NativeStatus::Ok | NativeStatus::ShutdownForced) {
                Observation::phase("Stopped", generation)
            } else {
                Observation::failed(result, generation)
            };
            publish(&observation);
            state.observation = observation;
            state.status = Some(result);
        }
        self.completed.notify_all();
    }

    pub(crate) fn wait_for(&self, budget: Duration) -> Option<NativeStatus> {
        let state = self.lock();
        let (mut state, _) = self
            .completed
            .wait_timeout_while(state, budget, |state| state.status.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.status.is_none() {
            state.observation = Observation::failed(
                NativeStatus::ShutdownTimeout,
                state.observation.daemon_generation,
            );
        }
        state.status
    }
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
