//! Unified lifecycle registry for every daemon-owned local or SSH shell.
//!
//! The registry is intentionally transport-agnostic. Local process owners and
//! SSH channel owners receive the same close signal and publish through the
//! same state machine; only [`ShellKindWire`] identifies the backend.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use haider_rpc::{ShellKindWire, ShellStatusWire, ShellWire};
use tokio::sync::{broadcast, watch};

const EVENT_CAPACITY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellRegistryEvent {
    Opened(ShellWire),
    State(ShellWire),
    Closed(ShellWire),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellRegistryError {
    NotFound(String),
    Poisoned,
    IdGeneration(String),
}

impl fmt::Display for ShellRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(formatter, "shell `{id}` was not found"),
            Self::Poisoned => formatter.write_str("shell registry is unavailable"),
            Self::IdGeneration(message) => write!(formatter, "cannot create shell id: {message}"),
        }
    }
}

impl std::error::Error for ShellRegistryError {}

struct ShellEntry {
    wire: ShellWire,
    close: watch::Sender<bool>,
}

struct ShellRegistryInner {
    entries: Mutex<BTreeMap<String, ShellEntry>>,
    events: broadcast::Sender<ShellRegistryEvent>,
}

/// Cloneable daemon-wide registry shared by local execution and SSH channels.
#[derive(Clone)]
pub(crate) struct ShellRegistry {
    inner: Arc<ShellRegistryInner>,
}

impl Default for ShellRegistry {
    fn default() -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            inner: Arc::new(ShellRegistryInner {
                entries: Mutex::new(BTreeMap::new()),
                events,
            }),
        }
    }
}

impl ShellRegistry {
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ShellRegistryEvent> {
        self.inner.events.subscribe()
    }

    pub(crate) fn list(&self) -> Result<Vec<ShellWire>, ShellRegistryError> {
        Ok(self
            .inner
            .entries
            .lock()
            .map_err(|_| ShellRegistryError::Poisoned)?
            .values()
            .map(|entry| entry.wire.clone())
            .collect())
    }

    #[cfg(test)]
    pub(crate) fn active_count(&self) -> usize {
        self.inner
            .entries
            .lock()
            .map(|entries| {
                entries
                    .values()
                    .filter(|entry| {
                        matches!(
                            &entry.wire.status,
                            ShellStatusWire::Starting | ShellStatusWire::Running
                        )
                    })
                    .count()
            })
            .unwrap_or_default()
    }

    pub(crate) fn open(
        &self,
        kind: ShellKindWire,
        title: impl Into<String>,
        cwd_or_host: impl Into<String>,
    ) -> Result<ShellHandle, ShellRegistryError> {
        let id = shell_id()?;
        let now = unix_ms();
        let wire = ShellWire {
            id: id.clone(),
            kind,
            status: ShellStatusWire::Starting,
            title: title.into(),
            cwd_or_host: cwd_or_host.into(),
            created_at_ms: now,
            last_activity_ms: now,
            bytes_out: 0,
        };
        let (close, close_rx) = watch::channel(false);
        self.inner
            .entries
            .lock()
            .map_err(|_| ShellRegistryError::Poisoned)?
            .insert(
                id.clone(),
                ShellEntry {
                    wire: wire.clone(),
                    close,
                },
            );
        let _ = self.inner.events.send(ShellRegistryEvent::Opened(wire));
        Ok(ShellHandle {
            id,
            registry: self.clone(),
            close_rx,
        })
    }

    pub(crate) fn close(&self, id: &str) -> Result<ShellWire, ShellRegistryError> {
        let mut entries = self
            .inner
            .entries
            .lock()
            .map_err(|_| ShellRegistryError::Poisoned)?;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ShellRegistryError::NotFound(id.to_owned()))?;
        if entry.wire.status != ShellStatusWire::Closed {
            entry.wire.status = ShellStatusWire::Closed;
            entry.wire.last_activity_ms = unix_ms();
            entry.close.send_replace(true);
            let wire = entry.wire.clone();
            drop(entries);
            let _ = self
                .inner
                .events
                .send(ShellRegistryEvent::Closed(wire.clone()));
            return Ok(wire);
        }
        Ok(entry.wire.clone())
    }

    fn update(
        &self,
        id: &str,
        apply: impl FnOnce(&mut ShellWire),
    ) -> Result<ShellWire, ShellRegistryError> {
        let mut entries = self
            .inner
            .entries
            .lock()
            .map_err(|_| ShellRegistryError::Poisoned)?;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ShellRegistryError::NotFound(id.to_owned()))?;
        if entry.wire.status == ShellStatusWire::Closed {
            return Ok(entry.wire.clone());
        }
        apply(&mut entry.wire);
        entry.wire.last_activity_ms = unix_ms();
        let wire = entry.wire.clone();
        drop(entries);
        let _ = self
            .inner
            .events
            .send(ShellRegistryEvent::State(wire.clone()));
        Ok(wire)
    }
}

/// Owner-side coordinate for publishing lifecycle changes and observing close.
pub(crate) struct ShellHandle {
    id: String,
    registry: ShellRegistry,
    close_rx: watch::Receiver<bool>,
}

impl ShellHandle {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn close_receiver(&self) -> watch::Receiver<bool> {
        self.close_rx.clone()
    }

    pub(crate) fn running(&self) -> Result<ShellWire, ShellRegistryError> {
        self.registry.update(&self.id, |wire| {
            wire.status = ShellStatusWire::Running;
        })
    }

    pub(crate) fn add_output(&self, bytes: usize) -> Result<ShellWire, ShellRegistryError> {
        self.registry.update(&self.id, |wire| {
            wire.bytes_out = wire
                .bytes_out
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        })
    }

    pub(crate) fn exited(&self, code: Option<i32>) -> Result<ShellWire, ShellRegistryError> {
        self.registry.update(&self.id, |wire| {
            wire.status = ShellStatusWire::Exited { code };
        })
    }
}

fn shell_id() -> Result<String, ShellRegistryError> {
    let mut random = [0_u8; 10];
    getrandom::fill(&mut random)
        .map_err(|error| ShellRegistryError::IdGeneration(error.to_string()))?;
    Ok(format!("sh-{}", hex::encode(random)))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "shell_registry_tests.rs"]
mod tests;
