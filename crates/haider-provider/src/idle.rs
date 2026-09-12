//! Last received progress for one logical request, including physical retries.

use crate::{ProviderError, ProviderErrorKind, ProviderTimeoutReason};
use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
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
    attempts: u64,
    cause: Option<ProviderError>,
    finished: bool,
}

/// Shared only by attempts of the same logical provider request. Opening an
/// attempt, returning response headers, and sleeping for backoff are not
/// progress. Native adapters report raw nonempty chunks before decoding.
#[derive(Debug, Clone, Default)]
pub struct ProviderIdleDeadline(Arc<Mutex<Option<State>>>);

impl ProviderIdleDeadline {
    pub fn begin_attempt(&self, budget: Option<Duration>) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.is_none() {
            let Some(budget) = budget else { return };
            let now = Instant::now();
            *state = Some(State {
                budget,
                started: now,
                last_progress: now,
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
    }

    pub fn observe_progress(&self) {
        if let Some(state) = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
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
            .0
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
            .0
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
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = state.as_ref()?;
        let now = Instant::now();
        if state.finished || now < state.last_progress + state.budget {
            return None;
        }
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let evidence = ProviderIdleTimeout {
            elapsed_ms: millis(now.saturating_duration_since(state.started)),
            idle_elapsed_ms: millis(now.saturating_duration_since(state.last_progress)),
            budget_ms: millis(state.budget),
            attempts: state.attempts,
            cause: state.cause.clone().map(Box::new),
        };
        let mut error = ProviderError::new(ProviderErrorKind::Transport,
            "provider operation received no bytes or frames within its idle budget")
            .with_timeout_reason(ProviderTimeoutReason::IdleTimeout)
            .with_presentation(ErrorPresentation::new("idle-timeout", "Provider idle timeout",
                "The provider stopped making progress. The idle budget includes retry attempts and backoff.",
                ErrorScope::Turn, [ErrorAction::Retry]));
        error.retryable = false;
        error.idle_timeout = Some(evidence);
        Some(error)
    }

    pub async fn wait(&self) -> ProviderError {
        loop {
            let deadline = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .filter(|state| !state.finished)
                .map(|state| state.last_progress + state.budget);
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
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
