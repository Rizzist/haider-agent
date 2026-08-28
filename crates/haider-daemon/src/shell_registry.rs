//! Unified lifecycle registry for every daemon-owned local or SSH shell.
//!
//! The registry is intentionally transport-agnostic. Local process owners and
//! SSH channel owners receive the same close signal and publish through the
//! same state machine; only [`ShellKindWire`] identifies the backend.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use haider_rpc::{
    ShellKindWire, ShellOutputStreamWire, ShellStatusWire, ShellWire, SshPtySizeWire,
};
use tokio::sync::{broadcast, mpsc, watch};
use zeroize::Zeroizing;

const EVENT_CAPACITY: usize = 256;
const CONTROL_CAPACITY: usize = 64;

/// Non-retained control messages for one interactive SSH channel. Input bytes
/// never enter a registry row, event, journal, or diagnostic.
pub(crate) enum ShellControl {
    Input(Zeroizing<Vec<u8>>),
    Resize(SshPtySizeWire),
    Eof,
}

impl fmt::Debug for ShellControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(_) => formatter.write_str("Input(<redacted>)"),
            Self::Resize(size) => formatter.debug_tuple("Resize").field(size).finish(),
            Self::Eof => formatter.write_str("Eof"),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ShellRegistryEvent {
    Opened(ShellWire),
    State(ShellWire),
    Closed(ShellWire),
    Output {
        owner: Option<String>,
        id: String,
        stream: ShellOutputStreamWire,
        bytes: Zeroizing<Vec<u8>>,
    },
}

impl fmt::Debug for ShellRegistryEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Opened(shell) => formatter.debug_tuple("Opened").field(shell).finish(),
            Self::State(shell) => formatter.debug_tuple("State").field(shell).finish(),
            Self::Closed(shell) => formatter.debug_tuple("Closed").field(shell).finish(),
            Self::Output {
                owner,
                id,
                stream,
                bytes,
            } => formatter
                .debug_struct("Output")
                .field("owner", owner)
                .field("id", id)
                .field("stream", stream)
                .field("bytes", &format_args!("<redacted:{} bytes>", bytes.len()))
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellRegistryError {
    NotFound(String),
    Poisoned,
    IdGeneration(String),
    NotInteractive(String),
    ControlDenied(String),
    ControlBusy(String),
    ControlClosed(String),
}

impl fmt::Display for ShellRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(formatter, "shell `{id}` was not found"),
            Self::Poisoned => formatter.write_str("shell registry is unavailable"),
            Self::IdGeneration(message) => write!(formatter, "cannot create shell id: {message}"),
            Self::NotInteractive(id) => write!(formatter, "shell `{id}` is not interactive"),
            Self::ControlDenied(id) => {
                write!(
                    formatter,
                    "interactive shell `{id}` belongs to another connection"
                )
            }
            Self::ControlBusy(id) => {
                write!(formatter, "interactive shell `{id}` control queue is full")
            }
            Self::ControlClosed(id) => {
                write!(
                    formatter,
                    "interactive shell `{id}` no longer accepts input"
                )
            }
        }
    }
}

impl std::error::Error for ShellRegistryError {}

struct ShellEntry {
    wire: ShellWire,
    close: watch::Sender<bool>,
    control: Option<mpsc::Sender<ShellControl>>,
    owner: Option<String>,
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
        self.open_entry(kind, title.into(), cwd_or_host.into(), None, None)
    }

    pub(crate) fn open_interactive(
        &self,
        kind: ShellKindWire,
        title: impl Into<String>,
        cwd_or_host: impl Into<String>,
        owner: Option<String>,
    ) -> Result<(ShellHandle, mpsc::Receiver<ShellControl>), ShellRegistryError> {
        let (control, receiver) = mpsc::channel(CONTROL_CAPACITY);
        let handle =
            self.open_entry(kind, title.into(), cwd_or_host.into(), Some(control), owner)?;
        Ok((handle, receiver))
    }

    fn open_entry(
        &self,
        kind: ShellKindWire,
        title: String,
        cwd_or_host: String,
        control: Option<mpsc::Sender<ShellControl>>,
        owner: Option<String>,
    ) -> Result<ShellHandle, ShellRegistryError> {
        let id = shell_id()?;
        let now = unix_ms();
        let wire = ShellWire {
            id: id.clone(),
            kind,
            status: ShellStatusWire::Starting,
            title,
            cwd_or_host,
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
                    control,
                    owner: owner.clone(),
                },
            );
        let _ = self.inner.events.send(ShellRegistryEvent::Opened(wire));
        Ok(ShellHandle {
            id,
            registry: self.clone(),
            close_rx,
            output_owner: owner,
        })
    }

    pub(crate) fn get(&self, id: &str) -> Result<ShellWire, ShellRegistryError> {
        self.inner
            .entries
            .lock()
            .map_err(|_| ShellRegistryError::Poisoned)?
            .get(id)
            .map(|entry| entry.wire.clone())
            .ok_or_else(|| ShellRegistryError::NotFound(id.to_owned()))
    }

    pub(crate) fn control(
        &self,
        id: &str,
        owner: Option<&str>,
        message: ShellControl,
    ) -> Result<ShellWire, ShellRegistryError> {
        let sender = {
            let entries = self
                .inner
                .entries
                .lock()
                .map_err(|_| ShellRegistryError::Poisoned)?;
            let entry = entries
                .get(id)
                .ok_or_else(|| ShellRegistryError::NotFound(id.to_owned()))?;
            if entry.owner.is_some() && entry.owner.as_deref() != owner {
                return Err(ShellRegistryError::ControlDenied(id.to_owned()));
            }
            if !matches!(
                &entry.wire.status,
                ShellStatusWire::Starting | ShellStatusWire::Running
            ) {
                return Err(ShellRegistryError::ControlClosed(id.to_owned()));
            }
            entry
                .control
                .clone()
                .ok_or_else(|| ShellRegistryError::NotInteractive(id.to_owned()))?
        };
        sender.try_send(message).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ShellRegistryError::ControlBusy(id.to_owned()),
            mpsc::error::TrySendError::Closed(_) => {
                ShellRegistryError::ControlClosed(id.to_owned())
            }
        })?;
        self.get(id)
    }

    pub(crate) fn close_control(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> Result<ShellWire, ShellRegistryError> {
        {
            let entries = self
                .inner
                .entries
                .lock()
                .map_err(|_| ShellRegistryError::Poisoned)?;
            let entry = entries
                .get(id)
                .ok_or_else(|| ShellRegistryError::NotFound(id.to_owned()))?;
            if entry.owner.is_some() && entry.owner.as_deref() != owner {
                return Err(ShellRegistryError::ControlDenied(id.to_owned()));
            }
        }
        self.close(id)
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

    pub(crate) fn close_owner(&self, owner: &str) -> Result<(), ShellRegistryError> {
        let ids = self
            .inner
            .entries
            .lock()
            .map_err(|_| ShellRegistryError::Poisoned)?
            .iter()
            .filter(|(_, entry)| {
                entry.owner.as_deref() == Some(owner)
                    && matches!(
                        &entry.wire.status,
                        ShellStatusWire::Starting | ShellStatusWire::Running
                    )
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.close(&id)?;
        }
        Ok(())
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
#[derive(Clone)]
pub(crate) struct ShellHandle {
    id: String,
    registry: ShellRegistry,
    close_rx: watch::Receiver<bool>,
    output_owner: Option<String>,
}

impl ShellHandle {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn close_receiver(&self) -> watch::Receiver<bool> {
        self.close_rx.clone()
    }

    pub(crate) fn running(&self) -> Result<ShellWire, ShellRegistryError> {
        let wire = self.registry.update(&self.id, |wire| {
            wire.status = ShellStatusWire::Running;
        })?;
        if wire.status == ShellStatusWire::Running {
            Ok(wire)
        } else {
            Err(ShellRegistryError::ControlClosed(self.id.clone()))
        }
    }

    pub(crate) fn add_output(&self, bytes: usize) -> Result<ShellWire, ShellRegistryError> {
        self.registry.update(&self.id, |wire| {
            wire.bytes_out = wire
                .bytes_out
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        })
    }

    pub(crate) fn publish_output(
        &self,
        stream: ShellOutputStreamWire,
        bytes: &[u8],
    ) -> Result<ShellWire, ShellRegistryError> {
        let wire = self.add_output(bytes.len())?;
        if !bytes.is_empty() {
            let _ = self.registry.inner.events.send(ShellRegistryEvent::Output {
                owner: self.output_owner.clone(),
                id: self.id.clone(),
                stream,
                bytes: Zeroizing::new(bytes.to_vec()),
            });
        }
        Ok(wire)
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
