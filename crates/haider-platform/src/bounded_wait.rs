//! Shared typed deadline for lifecycle tails and post-termination cleanup.

use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitTimeout {
    operation: &'static str,
    limit: Duration,
}

impl WaitTimeout {
    #[must_use]
    pub fn operation(self) -> &'static str {
        self.operation
    }

    #[must_use]
    pub fn limit(self) -> Duration {
        self.limit
    }
}

impl std::fmt::Display for WaitTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} did not complete within {} ms",
            self.operation,
            self.limit.as_millis()
        )
    }
}

#[derive(Debug)]
#[must_use = "a lifecycle deadline outcome must be handled explicitly"]
pub enum BoundedWait<T> {
    Completed(T),
    TimedOut(WaitTimeout),
}

pub async fn bounded_wait<T>(
    operation: &'static str,
    limit: Duration,
    future: impl Future<Output = T>,
) -> BoundedWait<T> {
    match tokio::time::timeout(limit, future).await {
        Ok(value) => BoundedWait::Completed(value),
        Err(_) => BoundedWait::TimedOut(WaitTimeout { operation, limit }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_lifecycle_wait_has_a_typed_timeout() {
        let outcome = bounded_wait(
            "test pending lifecycle wait",
            Duration::from_millis(1),
            std::future::pending::<()>(),
        )
        .await;
        let BoundedWait::TimedOut(timeout) = outcome else {
            panic!("pending wait unexpectedly completed")
        };
        assert_eq!(timeout.operation(), "test pending lifecycle wait");
        assert_eq!(timeout.limit(), Duration::from_millis(1));
    }
}
