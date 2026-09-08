//! A timeout observes completion without consuming or cancelling the owner.
use crate::contract::NativeStatus;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub(crate) struct CompletionReceipt {
    status: Mutex<Option<NativeStatus>>,
    completed: Condvar,
}

impl CompletionReceipt {
    pub(crate) fn get(&self) -> Option<NativeStatus> {
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The owner calls this only after runtime and blocking-reader teardown.
    pub(crate) fn finish(&self, result: NativeStatus) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if status.is_none() {
            *status = Some(result);
        }
        self.completed.notify_all();
    }

    pub(crate) fn wait_for(&self, budget: Duration) -> Option<NativeStatus> {
        let status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (status, _) = self
            .completed
            .wait_timeout_while(status, budget, |status| status.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *status
    }

    pub(crate) fn shutdown_result(&self, budget: Duration) -> NativeStatus {
        match self.wait_for(budget) {
            Some(NativeStatus::ShutdownForced) => NativeStatus::ShutdownForced,
            // Startup/runtime failure remains in Observe. A joined owner is
            // already stopped; it is not a shutdown timeout or live ownership.
            Some(_) => NativeStatus::Ok,
            None => NativeStatus::ShutdownTimeout,
        }
    }
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
