//! Last received progress for one logical request, including physical retries.

use crate::{ProviderError, ProviderErrorKind, ProviderTimeoutReason};
use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};

tokio::task_local! {
    static IDLE_DEADLINE: ProviderIdleDeadline;
}

/// Additive terminal evidence. The preceding transport failure stays intact;
/// an initially silent request has no preceding failure to report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderIdleTimeout {
    /// Active logical-request time; request-body upload intervals are excluded.
    /// This is not wall-clock time from the first attempt to the timeout.
    pub elapsed_ms: u64,
    pub idle_elapsed_ms: u64,
    pub budget_ms: u64,
    pub attempts: u64,
    pub cause: Option<Box<ProviderError>>,
}

impl ProviderIdleTimeout {
    /// Copy whose preceding cause carries only its public message, for the
    /// durable `haider.provider.idle_timeout.v1` extension.
    #[must_use]
    pub fn shareable(&self) -> Self {
        Self {
            cause: self.cause.as_ref().map(|cause| Box::new(cause.shareable())),
            ..self.clone()
        }
    }
}

#[derive(Debug)]
struct State {
    budget: Duration,
    started: Instant,
    last_progress: Instant,
    upload_pause_started: Option<Instant>,
    excluded_upload: Duration,
    attempts: u64,
    cause: Option<ProviderError>,
    finished: bool,
}

impl State {
    /// Ends an upload pause, shifting the idle deadline by the paused time
    /// so upload is charged to neither idle nor elapsed evidence.
    fn end_upload_pause(&mut self) {
        if let Some(paused_at) = self.upload_pause_started.take() {
            let excluded = Instant::now().saturating_duration_since(paused_at);
            self.excluded_upload = self.excluded_upload.saturating_add(excluded);
            self.last_progress += excluded;
        }
    }
}

/// Shared only by attempts of the same logical provider request. Opening an
/// attempt, returning response headers, and sleeping for backoff are not
/// progress. Native adapters report raw nonempty chunks before decoding.
#[derive(Debug, Clone, Default)]
pub struct ProviderIdleDeadline {
    state: Arc<Mutex<Option<State>>>,
    changed: Arc<Notify>,
}

impl ProviderIdleDeadline {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<State>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn begin_attempt(&self, budget: Option<Duration>) {
        let mut state = self.lock();
        if state.is_none() {
            let Some(budget) = budget else { return };
            let now = Instant::now();
            *state = Some(State {
                budget,
                started: now,
                last_progress: now,
                upload_pause_started: None,
                excluded_upload: Duration::ZERO,
                attempts: 0,
                cause: None,
                finished: false,
            });
        }
        if let Some(state) = state.as_mut()
            && Instant::now() < state.last_progress + state.budget
        {
            state.attempts = state.attempts.saturating_add(1);
        }
        drop(state);
        self.changed.notify_one();
    }

    /// Suspends the logical idle clock while the serialized request body is
    /// being uploaded. Response-open and provider-silence attribution begin
    /// only after the HTTP body producer reaches EOF.
    pub(crate) fn pause_for_upload(&self) {
        let mut state = self.lock();
        if let Some(state) = state.as_mut()
            && !state.finished
            && state.upload_pause_started.is_none()
            && Instant::now() < state.last_progress + state.budget
        {
            state.upload_pause_started = Some(Instant::now());
        }
        drop(state);
        self.changed.notify_one();
    }

    /// Resumes the logical idle clock without charging the upload interval.
    pub(crate) fn resume_after_upload(&self) {
        let mut state = self.lock();
        if let Some(state) = state.as_mut() {
            state.end_upload_pause();
        }
        drop(state);
        self.changed.notify_one();
    }

    pub fn observe_progress(&self) {
        if let Some(state) = self.lock().as_mut() {
            state.end_upload_pause();
            let now = Instant::now();
            // A late chunk cannot resurrect an already exhausted operation.
            if now < state.last_progress + state.budget {
                state.last_progress = now;
            }
        }
    }

    pub fn record_error(&self, error: &ProviderError) {
        if error.timeout_reason == Some(ProviderTimeoutReason::IdleTimeout) {
            return;
        }
        if let Some(state) = self.lock().as_mut() {
            state.cause = Some(error.clone());
        }
    }

    /// A received terminal frame ends the transport wait, even if core is
    /// still processing queued frames or tools. Local work is not provider
    /// silence. A terminal received after expiry cannot revive the operation.
    pub(crate) fn observe_items(&self, items: &[crate::ProviderStreamItem]) {
        if !items.iter().any(|item| {
            matches!(
                item,
                Ok(haider_protocol::provider::StreamEvent::Finish { .. })
            )
        }) {
            return;
        }
        if let Some(state) = self.lock().as_mut()
            && Instant::now() < state.last_progress + state.budget
        {
            state.finished = true;
        }
    }

    pub fn expired(&self) -> Option<ProviderError> {
        let state = self.lock();
        let state = state.as_ref()?;
        let now = Instant::now();
        if state.finished
            || state.upload_pause_started.is_some()
            || now < state.last_progress + state.budget
        {
            return None;
        }
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let evidence = ProviderIdleTimeout {
            elapsed_ms: millis(
                now.saturating_duration_since(state.started)
                    .saturating_sub(state.excluded_upload),
            ),
            idle_elapsed_ms: millis(now.saturating_duration_since(state.last_progress)),
            budget_ms: millis(state.budget),
            attempts: state.attempts,
            cause: state.cause.clone().map(Box::new),
        };
        let cause_message = evidence
            .cause
            .as_ref()
            .map(|cause| cause.message.as_str())
            .unwrap_or("the provider returned no response bytes or frames");
        let cause_message = crate::bounded_context_field(cause_message, 512);
        let message = format!(
            "provider operation exhausted its {}ms active idle budget after {} attempt(s); last cause: {cause_message}",
            evidence.budget_ms, evidence.attempts
        );
        let detail = format!(
            "The provider stopped making progress after request upload was excluded from the idle clock. Last cause: {cause_message}"
        );
        let mut error = ProviderError::new(ProviderErrorKind::Transport, message)
            .with_timeout_reason(ProviderTimeoutReason::IdleTimeout)
            .with_presentation(ErrorPresentation::new(
                "idle-timeout",
                "Provider idle timeout",
                &detail,
                ErrorScope::Turn,
                [ErrorAction::Retry],
            ));
        error.retryable = false;
        error.idle_timeout = Some(evidence);
        Some(error)
    }

    pub async fn wait(&self) -> ProviderError {
        loop {
            let changed = self.changed.notified();
            let deadline = self
                .lock()
                .as_ref()
                .filter(|state| !state.finished && state.upload_pause_started.is_none())
                .map(|state| state.last_progress + state.budget);
            match deadline {
                Some(deadline) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(deadline) => {},
                        () = changed => continue,
                    }
                }
                None => changed.await,
            }
            if let Some(error) = self.expired() {
                return error;
            }
        }
    }

    pub async fn scope<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        IDLE_DEADLINE.scope(self.clone(), future).await
    }

    pub(crate) fn current() -> Option<Self> {
        IDLE_DEADLINE.try_with(Clone::clone).ok()
    }
}

#[cfg(test)]
#[path = "idle_tests.rs"]
mod tests;
