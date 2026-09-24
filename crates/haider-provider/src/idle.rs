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
    pub elapsed_ms: u64,
    pub idle_elapsed_ms: u64,
    pub budget_ms: u64,
    pub attempts: u64,
    pub cause: Option<Box<ProviderError>>,
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

/// Shared only by attempts of the same logical provider request. Opening an
/// attempt, returning response headers, and sleeping for backoff are not
/// progress. Native adapters report raw nonempty chunks before decoding.
#[derive(Debug, Clone, Default)]
pub struct ProviderIdleDeadline {
    state: Arc<Mutex<Option<State>>>,
    changed: Arc<Notify>,
}

impl ProviderIdleDeadline {
    pub fn begin_attempt(&self, budget: Option<Duration>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = state.as_mut()
            && let Some(paused_at) = state.upload_pause_started.take()
        {
            let excluded = Instant::now().saturating_duration_since(paused_at);
            state.excluded_upload = state.excluded_upload.saturating_add(excluded);
            state.last_progress += excluded;
        }
        drop(state);
        self.changed.notify_one();
    }

    pub fn observe_progress(&self) {
        if let Some(state) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            if let Some(paused_at) = state.upload_pause_started.take() {
                let excluded = Instant::now().saturating_duration_since(paused_at);
                state.excluded_upload = state.excluded_upload.saturating_add(excluded);
                state.last_progress += excluded;
            }
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
        if let Some(state) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
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
        if let Some(state) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            && Instant::now() < state.last_progress + state.budget
        {
            state.finished = true;
        }
    }

    pub fn expired(&self) -> Option<ProviderError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
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
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn request_upload_does_not_consume_idle_budget() {
        let idle = ProviderIdleDeadline::default();
        idle.begin_attempt(Some(Duration::from_secs(30)));
        idle.pause_for_upload();
        tokio::time::advance(Duration::from_secs(45)).await;
        assert!(idle.expired().is_none());

        idle.resume_after_upload();
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(idle.expired().is_none());
        tokio::time::advance(Duration::from_secs(1)).await;
        let error = match idle.expired() {
            Some(error) => error,
            None => panic!("active idle budget did not expire"),
        };
        let evidence = match error.idle_timeout {
            Some(evidence) => evidence,
            None => panic!("idle timeout omitted typed evidence"),
        };
        assert_eq!(evidence.idle_elapsed_ms, 30_000);
        assert_eq!(evidence.elapsed_ms, 30_000);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_idle_error_names_the_last_response_open_cause() {
        let idle = ProviderIdleDeadline::default();
        idle.begin_attempt(Some(Duration::from_secs(1)));
        idle.record_error(&ProviderError::new(
            ProviderErrorKind::Transport,
            "Anthropic response did not open within 60 seconds",
        ));
        tokio::time::advance(Duration::from_secs(1)).await;

        let error = match idle.expired() {
            Some(error) => error,
            None => panic!("idle timeout did not expire"),
        };
        assert!(error.message.contains("Anthropic response did not open"));
        assert!(
            error
                .presentation
                .detail
                .contains("Anthropic response did not open")
        );
    }
}
