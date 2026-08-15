#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    Terminate,
    Interrupt,
    ConsoleClose,
}

impl ShutdownSignal {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Terminate => "SIGTERM",
            Self::Interrupt => "SIGINT",
            Self::ConsoleClose => "CTRL_CLOSE",
        }
    }
}

#[derive(Debug)]
pub struct ShutdownInstallError {
    signal: &'static str,
    source: std::io::Error,
}

impl ShutdownInstallError {
    #[must_use]
    pub const fn signal(&self) -> &'static str {
        self.signal
    }
}

impl std::fmt::Display for ShutdownInstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for ShutdownInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(unix)]
pub struct ShutdownSignals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    pub fn new() -> Result<Self, ShutdownInstallError> {
        let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|source| ShutdownInstallError {
            signal: "SIGTERM",
            source,
        })?;
        let interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(|source| ShutdownInstallError {
            signal: "SIGINT",
            source,
        })?;
        Ok(Self {
            terminate,
            interrupt,
        })
    }

    async fn recv(&mut self) -> Option<ShutdownSignal> {
        tokio::select! {
            signal = self.terminate.recv() => signal.map(|()| ShutdownSignal::Terminate),
            signal = self.interrupt.recv() => signal.map(|()| ShutdownSignal::Interrupt),
        }
    }
}

#[cfg(windows)]
pub struct ShutdownSignals {
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_close: tokio::signal::windows::CtrlClose,
}

#[cfg(windows)]
impl ShutdownSignals {
    pub fn new() -> Result<Self, ShutdownInstallError> {
        let ctrl_c = tokio::signal::windows::ctrl_c().map_err(|source| ShutdownInstallError {
            signal: "CTRL_C",
            source,
        })?;
        let ctrl_close =
            tokio::signal::windows::ctrl_close().map_err(|source| ShutdownInstallError {
                signal: "CTRL_CLOSE",
                source,
            })?;
        Ok(Self { ctrl_c, ctrl_close })
    }

    async fn recv(&mut self) -> Option<ShutdownSignal> {
        tokio::select! {
            signal = self.ctrl_c.recv() => signal.map(|()| ShutdownSignal::Interrupt),
            signal = self.ctrl_close.recv() => signal.map(|()| ShutdownSignal::ConsoleClose),
        }
    }
}

/// Waits for the next event from one persistently installed OS signal set.
/// Keeping the receiver outside this future preserves rapid second-signal
/// delivery and therefore the daemon's forced-shutdown law.
pub async fn shutdown_signal(signals: &mut ShutdownSignals) -> Option<ShutdownSignal> {
    signals.recv().await
}
