//! Keep paused fixture time fixed while workers perform real SQLite/image work.

pub(crate) struct FrozenClock(tokio::task::JoinHandle<()>);

impl FrozenClock {
    /// Use inside a `start_paused` test whose assertions await durable events.
    /// A runnable task prevents Tokio from auto-advancing past policy timers
    /// while a blocking worker is still running. The outer test runner bounds
    /// hangs independently of this virtual clock.
    pub(crate) fn hold() -> Self {
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }
}

impl Drop for FrozenClock {
    fn drop(&mut self) {
        self.0.abort();
    }
}
