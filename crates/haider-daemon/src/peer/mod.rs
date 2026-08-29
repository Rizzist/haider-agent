//! Profile-local peer discovery and durable turn-boundary delivery.

use crate::session_hub::{SessionHub, SessionHubError, WeakSessionHub};
use haider_core::AcceptedTurn;
use haider_protocol::ids::SessionId;
#[cfg(unix)]
use haider_protocol::peer::{PEER_FRAME_MAX_BYTES, PeerWireBody, PeerWireFrame};
use haider_protocol::peer::{
    PEER_ID_MAX_BYTES, PEER_MESSAGE_MAX_BYTES, PEER_MSG_ID_MAX_BYTES, PEER_NAME_MAX_BYTES,
    PEER_SUMMARY_MAX_BYTES, PEER_WIRE_VERSION, PeerCandidate, PeerDelivery, PeerDeliveryReason,
    PeerDescriptor, PeerKind, PeerManifest, PeerMessage, PeerReceipt, PeerSender, PeerState,
    PeerTrust,
};
use haider_rpc::{ObserveRunStateWire, SessionSummary, WireFrame};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::Notify;
#[cfg(unix)]
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[cfg(unix)]
use haider_platform::{BoundEndpoint, Endpoint, PeerEndpointKind, peer_endpoint_paths};
#[cfg(windows)]
use haider_platform::{PeerEndpointKind, peer_endpoint_paths};

const MESSAGE_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const PEER_ADDRESS_MAX_BYTES: usize = PEER_ID_MAX_BYTES + PEER_NAME_MAX_BYTES + 3;
const RECONCILE_INTERVAL: Duration = Duration::from_millis(500);
const MANIFEST_HEARTBEAT_MS: u64 = 5_000;
const MANIFEST_SCALAR_MAX_BYTES: usize = 4_096;
#[cfg(unix)]
const WIRE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const MANIFEST_MAX_BYTES: u64 = 16 * 1024;

#[derive(Debug)]
pub(crate) enum PeerError {
    Ambiguous {
        candidates: Vec<PeerCandidate>,
    },
    Invalid {
        message: String,
    },
    Unavailable {
        message: String,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    Platform(haider_platform::EndpointError),
    Hub(SessionHubError),
}

impl PeerError {
    fn io(operation: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ambiguous { .. } => formatter.write_str("peer address is ambiguous"),
            Self::Invalid { message } | Self::Unavailable { message } => {
                formatter.write_str(message)
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
            Self::Platform(error) => write!(formatter, "peer endpoint: {error}"),
            Self::Hub(error) => write!(formatter, "peer session hub: {error}"),
        }
    }
}

impl std::error::Error for PeerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Platform(error) => Some(error),
            Self::Hub(error) => Some(error),
            _ => None,
        }
    }
}

impl From<haider_platform::EndpointError> for PeerError {
    fn from(error: haider_platform::EndpointError) -> Self {
        Self::Platform(error)
    }
}

impl From<SessionHubError> for PeerError {
    fn from(error: SessionHubError) -> Self {
        Self::Hub(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum MailboxRecord {
    Queued {
        message: PeerMessage,
    },
    /// Target-owned admission claim. This is appended under the mailbox
    /// lease before touching the target's core store, so a daemon with a
    /// different store can never expire an ambiguously committed turn.
    Claimed {
        msg_id: String,
    },
    Accepted {
        msg_id: String,
        accepted: AcceptedTurn,
    },
    Terminal {
        receipt: PeerReceipt,
    },
    TargetPublished {
        msg_id: String,
    },
    Published {
        msg_id: String,
    },
    Receipt {
        receipt: PeerReceipt,
    },
    #[cfg(unix)]
    Outbound {
        msg_id: String,
        target_id: String,
        target_kind: PeerKind,
        expires_at: u64,
    },
}

#[derive(Debug)]
pub(super) struct PendingMessage {
    pub(super) message: PeerMessage,
    pub(super) claimed: bool,
    pub(super) accepted: Option<AcceptedTurn>,
    pub(super) terminal: Option<PeerReceipt>,
    pub(super) target_published: bool,
    pub(super) published: bool,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OutboundReceiptState {
    Outstanding {
        target_kind: PeerKind,
        expires_at: u64,
    },
    Journaled(PeerReceipt),
}

#[cfg(unix)]
struct LocalPublication {
    descriptor: PeerDescriptor,
    paths: haider_platform::PeerEndpointPaths,
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

#[cfg(windows)]
struct LocalPublication {
    descriptor: PeerDescriptor,
    paths: haider_platform::PeerEndpointPaths,
}

#[cfg(unix)]
struct MailboxLease {
    _file: std::fs::File,
}

#[cfg(windows)]
struct MailboxLease;

/// One profile daemon's peer registry, socket listeners, and mailbox pump.
pub(crate) struct PeerService {
    runtime_dir: PathBuf,
    hub: WeakSessionHub,
    draining: AtomicBool,
    recovering: AtomicBool,
    wake: Notify,
    reconcile_serial: tokio::sync::Mutex<()>,
    delivery_serial: tokio::sync::Mutex<()>,
    mailbox_serial: tokio::sync::Mutex<()>,
    publications: Mutex<HashMap<String, LocalPublication>>,
    background: Mutex<Option<JoinHandle<()>>>,
}

impl PeerService {
    pub(crate) async fn start(
        runtime_dir: PathBuf,
        hub: &SessionHub,
    ) -> Result<Arc<Self>, PeerError> {
        let service = Arc::new(Self {
            runtime_dir,
            hub: hub.downgrade(),
            draining: AtomicBool::new(false),
            recovering: AtomicBool::new(true),
            wake: Notify::new(),
            reconcile_serial: tokio::sync::Mutex::new(()),
            delivery_serial: tokio::sync::Mutex::new(()),
            mailbox_serial: tokio::sync::Mutex::new(()),
            publications: Mutex::new(HashMap::new()),
            background: Mutex::new(None),
        });
        service.reconcile_once().await?;
        service.recovering.store(false, Ordering::Release);
        let weak = Arc::downgrade(&service);
        let task = tokio::spawn(async move {
            loop {
                let Some(service) = weak.upgrade() else {
                    return;
                };
                if service.draining.load(Ordering::Acquire) {
                    return;
                }
                if let Err(error) = service.reconcile_once().await {
                    tracing::warn!(target: "haider.peer", %error, "peer reconciliation failed");
                }
                tokio::select! {
                    () = tokio::time::sleep(RECONCILE_INTERVAL) => {}
                    () = service.wake.notified() => {}
                }
            }
        });
        let mut background = service
            .background
            .lock()
            .map_err(|_| PeerError::Unavailable {
                message: "peer background-task registry is poisoned".into(),
            })?;
        *background = Some(task);
        drop(background);
        Ok(service)
    }

    pub(crate) fn begin_draining(&self) {
        if self.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        self.wake.notify_waiters();
        #[cfg(unix)]
        if let Ok(publications) = self.publications.lock() {
            for publication in publications.values() {
                let _ = publication.cancel.send(true);
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.begin_draining();
        let background = self.background.lock().ok().and_then(|mut task| task.take());
        if let Some(task) = background {
            let _ = task.await;
        }
        let publications: Vec<LocalPublication> = self
            .publications
            .lock()
            .map(|mut publications| publications.drain().map(|(_, value)| value).collect())
            .unwrap_or_default();
        #[cfg(unix)]
        for publication in publications {
            let _ = publication.cancel.send(true);
            let _ = publication.task.await;
            remove_manifest(&publication.paths.manifest).await;
        }
        #[cfg(windows)]
        for publication in publications {
            remove_manifest(&publication.paths.manifest).await;
        }
    }

    pub(crate) async fn list(self: &Arc<Self>) -> Result<Vec<PeerDescriptor>, PeerError> {
        self.ensure_running()?;
        self.reconcile_once().await?;
        self.discover().await
    }

    pub(crate) async fn send(
        self: &Arc<Self>,
        from: &SessionId,
        to: String,
        message: String,
        summary: Option<String>,
    ) -> Result<PeerReceipt, PeerError> {
        self.ensure_running()?;
        validate_text("peer address", &to, PEER_ADDRESS_MAX_BYTES, false)?;
        validate_header("peer address", &to)?;
        validate_text("peer message", &message, PEER_MESSAGE_MAX_BYTES, false)?;
        if let Some(summary) = summary.as_deref() {
            validate_text("peer summary", summary, PEER_SUMMARY_MAX_BYTES, true)?;
        }
        self.reconcile_once().await?;
        let agents = self.discover().await?;
        // Local publications are authoritative for Haider sender identity.
        // A same-UID external manifest must never be able to rename a local
        // session or acquire verified provenance by reusing its id.
        let sender = self
            .publications
            .lock()
            .map_err(|_| PeerError::Unavailable {
                message: "peer publication registry is poisoned".into(),
            })?
            .get(from.as_str())
            .map(|publication| publication.descriptor.clone())
            .ok_or_else(|| PeerError::Unavailable {
                message: format!("sender session {from} is not a live peer"),
            })?;
        let queued_at = now_ms();
        let msg_id = random_id("msg")?;
        let target = match resolve_address(&to, &agents) {
            Ok(target) => target,
            Err(PeerError::Unavailable { .. }) => {
                let receipt = receipt(
                    &msg_id,
                    PeerDelivery::Refused,
                    Some(PeerDeliveryReason::TargetUnavailable),
                );
                self.record_sender_receipt(&sender.id, receipt.clone())
                    .await?;
                return Ok(receipt);
            }
            Err(error) => return Err(error),
        };
        let message = PeerMessage {
            msg_id: msg_id.clone(),
            from: PeerSender {
                id: sender.id.clone(),
                name: sender.name.clone(),
                kind: PeerKind::HaiderSession,
                trust: PeerTrust::VerifiedHaider,
            },
            to: target.id.clone(),
            message,
            summary,
            queued_at,
            expires_at: queued_at.saturating_add(MESSAGE_TTL_MS),
        };
        if self.is_local(&target.id)? {
            let receipt = self.enqueue_local(message).await?;
            if receipt.delivery == PeerDelivery::Queued {
                self.record_sender_receipt(&sender.id, receipt.clone())
                    .await?;
            }
            return Ok(receipt);
        }
        #[cfg(unix)]
        {
            let path = endpoint_path_for(&self.runtime_dir, &target)?;
            self.record_outbound(
                &sender.id,
                &msg_id,
                &target.id,
                target.kind,
                message.expires_at,
            )
            .await?;
            // A timeout is deliberately not converted into Refused: the
            // remote may already have durably queued the delivery.
            let receipt = exchange_delivery(&path, PeerWireFrame::deliver(message)).await?;
            if receipt.delivery == PeerDelivery::Queued {
                self.record_sender_receipt(&sender.id, receipt.clone())
                    .await?;
            } else {
                // Terminal replies compete with the expiry pump and must use
                // its correlated, single-terminal journal transition.
                self.accept_wire_receipt(&sender.id, receipt.clone())
                    .await?;
            }
            Ok(receipt)
        }
        #[cfg(windows)]
        {
            let receipt = receipt(
                &msg_id,
                PeerDelivery::Refused,
                Some(PeerDeliveryReason::TargetUnavailable),
            );
            self.record_sender_receipt(&sender.id, receipt.clone())
                .await?;
            Ok(receipt)
        }
    }

    fn ensure_running(&self) -> Result<(), PeerError> {
        if self.draining.load(Ordering::Acquire) {
            Err(PeerError::Unavailable {
                message: "peer messaging is draining".into(),
            })
        } else {
            Ok(())
        }
    }

    fn hub(&self) -> Result<SessionHub, PeerError> {
        self.hub.upgrade().ok_or_else(|| PeerError::Unavailable {
            message: "peer session hub is no longer available".into(),
        })
    }

    fn is_local(&self, id: &str) -> Result<bool, PeerError> {
        self.publications
            .lock()
            .map(|publications| publications.contains_key(id))
            .map_err(|_| PeerError::Unavailable {
                message: "peer publication registry is poisoned".into(),
            })
    }

    async fn reconcile_once(self: &Arc<Self>) -> Result<(), PeerError> {
        let _serial = self.reconcile_serial.lock().await;
        if self.draining.load(Ordering::Acquire) {
            return Ok(());
        }
        let summaries = self.hub()?.peer_session_summaries().await?;
        let now = now_ms();
        let desired = summaries
            .into_iter()
            .map(|summary| {
                let descriptor = descriptor_from_summary(summary, now);
                (descriptor.id.clone(), descriptor)
            })
            .collect::<HashMap<_, _>>();
        let existing = self
            .publications
            .lock()
            .map_err(|_| PeerError::Unavailable {
                message: "peer publication registry is poisoned".into(),
            })?
            .keys()
            .cloned()
            .collect::<HashSet<_>>();

        for (id, descriptor) in &desired {
            if existing.contains(id) {
                let paths = {
                    let mut publications =
                        self.publications
                            .lock()
                            .map_err(|_| PeerError::Unavailable {
                                message: "peer publication registry is poisoned".into(),
                            })?;
                    let publication =
                        publications
                            .get_mut(id)
                            .ok_or_else(|| PeerError::Unavailable {
                                message: format!("peer publication {id} disappeared"),
                            })?;
                    let changed = descriptor_changed(&publication.descriptor, descriptor);
                    let heartbeat_due = now.saturating_sub(publication.descriptor.last_seen)
                        >= MANIFEST_HEARTBEAT_MS;
                    if changed || heartbeat_due {
                        publication.descriptor = descriptor.clone();
                        Some(publication.paths.clone())
                    } else {
                        None
                    }
                };
                if let Some(paths) = paths {
                    write_manifest(&paths, descriptor).await?;
                }
            } else {
                let publication = self.publish_local(descriptor.clone()).await?;
                self.publications
                    .lock()
                    .map_err(|_| PeerError::Unavailable {
                        message: "peer publication registry is poisoned".into(),
                    })?
                    .insert(id.clone(), publication);
            }
        }

        let removed = existing
            .iter()
            .filter(|id| !desired.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in removed {
            self.expire_target(&id, PeerDeliveryReason::TargetUnavailable)
                .await?;
            let publication = self
                .publications
                .lock()
                .ok()
                .and_then(|mut publications| publications.remove(&id));
            if let Some(publication) = publication {
                #[cfg(unix)]
                {
                    let _ = publication.cancel.send(true);
                    let _ = publication.task.await;
                }
                remove_manifest(&publication.paths.manifest).await;
            }
        }
        self.process_mailboxes().await
    }

    async fn publish_local(
        self: &Arc<Self>,
        descriptor: PeerDescriptor,
    ) -> Result<LocalPublication, PeerError> {
        let paths =
            peer_endpoint_paths(&self.runtime_dir, &descriptor.id, PeerEndpointKind::Haider)?;
        #[cfg(unix)]
        {
            let endpoint = Endpoint::from_address(paths.socket.clone());
            let bound = BoundEndpoint::bind(&endpoint, &self.runtime_dir).await?;
            write_manifest(&paths, &descriptor).await?;
            let (cancel, cancelled) = watch::channel(false);
            let weak = Arc::downgrade(self);
            let target_id = descriptor.id.clone();
            let task = tokio::spawn(listener_loop(bound, weak, target_id, cancelled));
            Ok(LocalPublication {
                descriptor,
                paths,
                cancel,
                task,
            })
        }
        #[cfg(windows)]
        {
            write_manifest(&paths, &descriptor).await?;
            Ok(LocalPublication { descriptor, paths })
        }
    }

    async fn discover(&self) -> Result<Vec<PeerDescriptor>, PeerError> {
        #[cfg(unix)]
        {
            let _ = haider_platform::sweep_stale_endpoints(&self.runtime_dir, None).await;
            discover_unix(&self.runtime_dir).await
        }
        #[cfg(windows)]
        {
            let agents = self
                .publications
                .lock()
                .map_err(|_| PeerError::Unavailable {
                    message: "peer publication registry is poisoned".into(),
                })?
                .values()
                .map(|publication| publication.descriptor.clone())
                .collect::<Vec<_>>();
            Ok(deduplicate_agents(agents))
        }
    }

    pub(super) async fn enqueue_local(
        self: &Arc<Self>,
        mut message: PeerMessage,
    ) -> Result<PeerReceipt, PeerError> {
        let _delivery = self.delivery_serial.lock().await;
        normalize_incoming_message(&mut message)?;
        if !self.is_local(&message.to)? {
            let receipt = receipt(
                &message.msg_id,
                PeerDelivery::Refused,
                Some(PeerDeliveryReason::TargetUnavailable),
            );
            if self.is_local(&message.from.id)? {
                self.record_sender_receipt(&message.from.id, receipt.clone())
                    .await?;
            }
            return Ok(receipt);
        }
        let paths = peer_endpoint_paths(&self.runtime_dir, &message.to, PeerEndpointKind::Haider)?;
        let lease = self.lock_mailbox(&paths.mailbox).await?;
        if let Some(existing) = self
            .load_pending_repairing(&paths.mailbox)
            .await?
            .get(&message.msg_id)
        {
            if !same_delivery(&existing.message, &message) {
                return Err(PeerError::Invalid {
                    message: format!(
                        "peer message id {:?} was reused with different content",
                        message.msg_id
                    ),
                });
            }
            return Ok(existing
                .terminal
                .clone()
                .unwrap_or_else(|| receipt(&message.msg_id, PeerDelivery::Queued, None)));
        }
        self.append_record(
            &paths.mailbox,
            MailboxRecord::Queued {
                message: message.clone(),
            },
        )
        .await?;
        match self
            .process_one_locked(&paths.mailbox, message.clone(), lease)
            .await
        {
            Ok(receipt) => Ok(receipt),
            Err(error) => {
                // The queue append is already durable. A transient hub or
                // shutdown race must not turn that committed state into a
                // false refusal at the sender.
                tracing::warn!(
                    target: "haider.peer",
                    msg_id = %message.msg_id,
                    %error,
                    "durable peer delivery remains queued for retry"
                );
                Ok(receipt(&message.msg_id, PeerDelivery::Queued, None))
            }
        }
    }

    #[cfg(unix)]
    async fn registered_wire_sender(&self, id: &str) -> Result<PeerSender, PeerError> {
        if self.is_local(id)? {
            return Err(PeerError::Invalid {
                message: format!("socket peer cannot claim local Haider session {id:?}"),
            });
        }
        let descriptor = self
            .discover()
            .await?
            .into_iter()
            .find(|agent| agent.id == id)
            .ok_or_else(|| PeerError::Invalid {
                message: format!("socket peer {id:?} has no live, unambiguous manifest"),
            })?;
        Ok(wire_sender_from_descriptor(descriptor))
    }

    async fn process_mailboxes(self: &Arc<Self>) -> Result<(), PeerError> {
        let _delivery = self.delivery_serial.lock().await;
        let live_publications = self
            .publications
            .lock()
            .map_err(|_| PeerError::Unavailable {
                message: "peer publication registry is poisoned".into(),
            })?
            .values()
            .map(|publication| {
                (
                    publication.descriptor.id.clone(),
                    publication.paths.mailbox.clone(),
                )
            })
            .collect::<Vec<_>>();
        let live_mailboxes = live_publications
            .iter()
            .map(|(_, mailbox)| mailbox.clone())
            .collect::<HashSet<_>>();
        let live_peer_ids = self
            .discover()
            .await?
            .into_iter()
            .map(|peer| peer.id)
            .collect::<HashSet<_>>();
        for (sender_id, mailbox) in &live_publications {
            self.expire_outbound(sender_id, mailbox).await?;
        }
        let runtime_dir = self.runtime_dir.clone();
        let mut mailboxes =
            tokio::task::spawn_blocking(move || mailbox_candidates_blocking(&runtime_dir))
                .await
                .map_err(|error| PeerError::Unavailable {
                    message: format!("peer mailbox scan task failed: {error}"),
                })??;
        mailboxes.extend(live_mailboxes.iter().cloned());
        for mailbox in mailboxes {
            let pending = self.load_pending(&mailbox).await?;
            for pending in pending.into_values() {
                let target_is_local = live_mailboxes.contains(&mailbox);
                if pending.published {
                    if target_is_local
                        && pending.claimed
                        && pending.accepted.is_none()
                        && pending
                            .terminal
                            .as_ref()
                            .is_some_and(|receipt| receipt.delivery == PeerDelivery::Delivered)
                    {
                        let _ = self.process_one(&mailbox, pending.message).await?;
                    }
                    continue;
                }
                if pending.terminal.is_some() {
                    if target_is_local
                        && pending.claimed
                        && pending.accepted.is_none()
                        && pending
                            .terminal
                            .as_ref()
                            .is_some_and(|receipt| receipt.delivery == PeerDelivery::Delivered)
                    {
                        let _ = self.process_one(&mailbox, pending.message).await?;
                        continue;
                    }
                    let _lease = self.lock_mailbox(&mailbox).await?;
                    let refreshed = self.load_pending_repairing(&mailbox).await?;
                    if let Some(refreshed) = refreshed.get(&pending.message.msg_id)
                        && let Some(receipt) = refreshed.terminal.clone()
                    {
                        self.retry_terminal(
                            &mailbox,
                            &refreshed.message,
                            receipt,
                            refreshed.target_published,
                        )
                        .await;
                    }
                    continue;
                }
                if !target_is_local && live_peer_ids.contains(&pending.message.to) {
                    continue;
                }
                if pending.claimed && !target_is_local {
                    let _lease = self.lock_mailbox(&mailbox).await?;
                    let refreshed = self.load_pending_repairing(&mailbox).await?;
                    let Some(refreshed) = refreshed.get(&pending.message.msg_id) else {
                        continue;
                    };
                    if let Some(receipt) = refreshed.terminal.clone() {
                        self.retry_terminal(
                            &mailbox,
                            &refreshed.message,
                            receipt,
                            refreshed.target_published,
                        )
                        .await;
                        continue;
                    }
                    if !refreshed.claimed {
                        continue;
                    }
                    // `Claimed` is shared proof that the target reached an
                    // idle boundary before touching its private core store.
                    // Delivery is therefore durable even when the claimant
                    // crashes before recording its private acceptance.
                    let delivered =
                        receipt(&refreshed.message.msg_id, PeerDelivery::Delivered, None);
                    self.append_record(
                        &mailbox,
                        MailboxRecord::Terminal {
                            receipt: delivered.clone(),
                        },
                    )
                    .await?;
                    self.retry_terminal(&mailbox, &refreshed.message, delivered, false)
                        .await;
                    continue;
                }
                if pending.accepted.is_some() {
                    if target_is_local {
                        let _lease = self.lock_mailbox(&mailbox).await?;
                        let refreshed = self.load_pending_repairing(&mailbox).await?;
                        if let Some(refreshed) = refreshed.get(&pending.message.msg_id) {
                            if let Some(receipt) = refreshed.terminal.clone() {
                                self.retry_terminal(
                                    &mailbox,
                                    &refreshed.message,
                                    receipt,
                                    refreshed.target_published,
                                )
                                .await;
                            } else if let Some(accepted) = refreshed.accepted.clone() {
                                let _ = self
                                    .finish_accepted_turn(&mailbox, &refreshed.message, accepted)
                                    .await?;
                            }
                        }
                    }
                    continue;
                }
                if !target_is_local && expiration_receipt(&pending.message, now_ms()).is_some() {
                    let lease = self.lock_mailbox(&mailbox).await?;
                    self.finish_foreign_expiry(&mailbox, &pending.message.msg_id, lease)
                        .await?;
                    continue;
                }
                if !target_is_local {
                    continue;
                }
                let _ = self.process_one(&mailbox, pending.message).await?;
            }
        }
        Ok(())
    }

    /// Re-folds under the cross-process mailbox lease before a foreign daemon
    /// expires a Haider target. A claim appended after the scanner's first
    /// observation is durable target ownership and must win over the stale
    /// expiry decision.
    async fn finish_foreign_expiry(
        &self,
        mailbox: &Path,
        msg_id: &str,
        _lease: MailboxLease,
    ) -> Result<(), PeerError> {
        let refreshed = self.load_pending_repairing(mailbox).await?;
        let Some(refreshed) = refreshed.get(msg_id) else {
            return Ok(());
        };
        if let Some(receipt) = refreshed.terminal.clone() {
            self.retry_terminal(
                mailbox,
                &refreshed.message,
                receipt,
                refreshed.target_published,
            )
            .await;
            return Ok(());
        }
        if refreshed.claimed {
            let delivered = receipt(&refreshed.message.msg_id, PeerDelivery::Delivered, None);
            self.append_record(
                mailbox,
                MailboxRecord::Terminal {
                    receipt: delivered.clone(),
                },
            )
            .await?;
            self.retry_terminal(mailbox, &refreshed.message, delivered, false)
                .await;
            return Ok(());
        }
        if refreshed.accepted.is_some() {
            return Ok(());
        }
        if let Some(accepted) = self.hub()?.peer_turn_receipt(&refreshed.message).await? {
            self.append_record(
                mailbox,
                MailboxRecord::Accepted {
                    msg_id: refreshed.message.msg_id.clone(),
                    accepted,
                },
            )
            .await?;
            let delivered = receipt(&refreshed.message.msg_id, PeerDelivery::Delivered, None);
            self.append_record(
                mailbox,
                MailboxRecord::Terminal {
                    receipt: delivered.clone(),
                },
            )
            .await?;
            self.retry_terminal(mailbox, &refreshed.message, delivered, false)
                .await;
            return Ok(());
        }
        let Some(receipt) = expiration_receipt(&refreshed.message, now_ms()) else {
            return Ok(());
        };
        self.append_record(
            mailbox,
            MailboxRecord::Terminal {
                receipt: receipt.clone(),
            },
        )
        .await?;
        self.retry_terminal(mailbox, &refreshed.message, receipt, false)
            .await;
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn finish_foreign_expiry_after_snapshot_for_test(
        &self,
        mailbox: &Path,
        msg_id: &str,
    ) -> Result<(), PeerError> {
        let lease = self.lock_mailbox(mailbox).await?;
        self.finish_foreign_expiry(mailbox, msg_id, lease).await
    }

    async fn process_one(
        self: &Arc<Self>,
        mailbox: &Path,
        message: PeerMessage,
    ) -> Result<PeerReceipt, PeerError> {
        let lease = self.lock_mailbox(mailbox).await?;
        self.process_one_locked(mailbox, message, lease).await
    }

    async fn process_one_locked(
        &self,
        mailbox: &Path,
        message: PeerMessage,
        _lease: MailboxLease,
    ) -> Result<PeerReceipt, PeerError> {
        let mut claimed = false;
        if let Some(pending) = self
            .load_pending_repairing(mailbox)
            .await?
            .get(&message.msg_id)
        {
            claimed = pending.claimed;
            if let Some(accepted) = pending.accepted.clone() {
                let finished = self
                    .finish_accepted_turn(mailbox, &pending.message, accepted)
                    .await?;
                return Ok(if finished {
                    receipt(&message.msg_id, PeerDelivery::Delivered, None)
                } else {
                    receipt(&message.msg_id, PeerDelivery::Queued, None)
                });
            }
            if let Some(receipt) = pending.terminal.clone()
                && (receipt.delivery != PeerDelivery::Delivered || !claimed)
            {
                return Ok(receipt);
            }
        }
        if let Some(accepted) = self.hub()?.peer_turn_receipt(&message).await? {
            self.append_record(
                mailbox,
                MailboxRecord::Accepted {
                    msg_id: message.msg_id.clone(),
                    accepted: accepted.clone(),
                },
            )
            .await?;
            let finished = self
                .finish_accepted_turn(mailbox, &message, accepted)
                .await?;
            return Ok(if finished {
                receipt(&message.msg_id, PeerDelivery::Delivered, None)
            } else {
                receipt(&message.msg_id, PeerDelivery::Queued, None)
            });
        }
        if !claimed && let Some(receipt) = expiration_receipt(&message, now_ms()) {
            self.append_record(
                mailbox,
                MailboxRecord::Terminal {
                    receipt: receipt.clone(),
                },
            )
            .await?;
            self.retry_terminal(mailbox, &message, receipt.clone(), false)
                .await;
            return Ok(receipt);
        }
        let Some(claim) = self.hub()?.begin_peer_turn_claim(&message).await? else {
            return Ok(receipt(&message.msg_id, PeerDelivery::Queued, None));
        };
        if !claimed {
            if let Some(receipt) = expiration_receipt(&message, now_ms()) {
                self.append_record(
                    mailbox,
                    MailboxRecord::Terminal {
                        receipt: receipt.clone(),
                    },
                )
                .await?;
                self.retry_terminal(mailbox, &message, receipt.clone(), false)
                    .await;
                return Ok(receipt);
            }
            self.append_record(
                mailbox,
                MailboxRecord::Claimed {
                    msg_id: message.msg_id.clone(),
                },
            )
            .await?;
        }
        let (accepted, fresh) = self
            .hub()?
            .accept_claimed_peer_turn(&message, claim)
            .await?;
        self.append_record(
            mailbox,
            MailboxRecord::Accepted {
                msg_id: message.msg_id.clone(),
                accepted: accepted.clone(),
            },
        )
        .await?;
        // Startup recovery handed only pre-existing core receipts to the
        // manager before PeerService started. A Claim with no core receipt is
        // admitted here as fresh work and must still be handed off while the
        // mailbox reconciliation flag is set.
        if (!self.recovering.load(Ordering::Acquire) || fresh)
            && let Err(error) = self.hub()?.handoff_peer_turn(accepted).await
        {
            tracing::warn!(
                target: "haider.peer",
                msg_id = %message.msg_id,
                %error,
                "accepted peer turn remains queued for manager handoff retry"
            );
            return Ok(receipt(&message.msg_id, PeerDelivery::Queued, None));
        }
        let delivered = receipt(&message.msg_id, PeerDelivery::Delivered, None);
        self.append_record(
            mailbox,
            MailboxRecord::Terminal {
                receipt: delivered.clone(),
            },
        )
        .await?;
        self.retry_terminal(mailbox, &message, delivered.clone(), false)
            .await;
        Ok(delivered)
    }

    async fn finish_accepted_turn(
        &self,
        mailbox: &Path,
        message: &PeerMessage,
        accepted: AcceptedTurn,
    ) -> Result<bool, PeerError> {
        if !self.recovering.load(Ordering::Acquire)
            && let Err(error) = self.hub()?.handoff_peer_turn(accepted).await
        {
            tracing::warn!(
                target: "haider.peer",
                msg_id = %message.msg_id,
                %error,
                "accepted peer turn remains queued for manager handoff retry"
            );
            return Ok(false);
        }
        let receipt = receipt(&message.msg_id, PeerDelivery::Delivered, None);
        self.append_record(
            mailbox,
            MailboxRecord::Terminal {
                receipt: receipt.clone(),
            },
        )
        .await?;
        self.retry_terminal(mailbox, message, receipt, false).await;
        Ok(true)
    }

    async fn retry_terminal(
        &self,
        mailbox: &Path,
        message: &PeerMessage,
        receipt: PeerReceipt,
        target_published: bool,
    ) {
        if let Err(error) = self
            .publish_terminal(mailbox, message, receipt, target_published)
            .await
        {
            tracing::warn!(
                target: "haider.peer",
                msg_id = %message.msg_id,
                %error,
                "terminal peer receipt remains durable for retry"
            );
        }
    }

    async fn publish_terminal(
        &self,
        mailbox: &Path,
        message: &PeerMessage,
        receipt: PeerReceipt,
        target_published: bool,
    ) -> Result<(), PeerError> {
        if receipt.delivery == PeerDelivery::Delivered && !target_published {
            if let Ok(hub) = self.hub() {
                hub.publish_peer_event(
                    &SessionId::new(message.to.clone()),
                    WireFrame::PeerMessageReceived {
                        message: message.clone(),
                    },
                );
            }
            self.append_record(
                mailbox,
                MailboxRecord::TargetPublished {
                    msg_id: message.msg_id.clone(),
                },
            )
            .await?;
        }
        self.publish_delivery(message, receipt).await?;
        self.append_record(
            mailbox,
            MailboxRecord::Published {
                msg_id: message.msg_id.clone(),
            },
        )
        .await
    }

    async fn publish_delivery(
        &self,
        message: &PeerMessage,
        receipt: PeerReceipt,
    ) -> Result<(), PeerError> {
        let hub = self.hub().ok();
        if message.from.kind == PeerKind::HaiderSession
            && message.from.trust == PeerTrust::VerifiedHaider
            && self.is_local(&message.from.id)?
        {
            self.record_sender_receipt(&message.from.id, receipt.clone())
                .await?;
            if let Some(hub) = hub {
                hub.publish_peer_event(
                    &SessionId::new(message.from.id.clone()),
                    WireFrame::PeerDeliveryChanged {
                        receipt: receipt.clone(),
                    },
                );
            }
            return Ok(());
        }
        #[cfg(unix)]
        if let Ok(agents) = self.discover().await
            && let Some(sender) = agents.iter().find(|agent| agent.id == message.from.id)
            && let Ok(path) = endpoint_path_for(&self.runtime_dir, sender)
        {
            return send_receipt(&path, receipt).await;
        }
        Err(PeerError::Unavailable {
            message: format!(
                "peer receipt sender {} is not currently reachable",
                message.from.id
            ),
        })
    }

    pub(crate) async fn expire_target(
        &self,
        target_id: &str,
        reason: PeerDeliveryReason,
    ) -> Result<(), PeerError> {
        let _delivery = self.delivery_serial.lock().await;
        let paths = peer_endpoint_paths(&self.runtime_dir, target_id, PeerEndpointKind::Haider)?;
        for pending in self.load_pending(&paths.mailbox).await?.into_values() {
            let _lease = self.lock_mailbox(&paths.mailbox).await?;
            let refreshed = self.load_pending_repairing(&paths.mailbox).await?;
            let Some(pending) = refreshed.get(&pending.message.msg_id) else {
                continue;
            };
            if let Some(receipt) = pending.terminal.clone() {
                self.retry_terminal(
                    &paths.mailbox,
                    &pending.message,
                    receipt,
                    pending.target_published,
                )
                .await;
            } else if let Some(accepted) = pending.accepted.clone() {
                let _ = self
                    .finish_accepted_turn(&paths.mailbox, &pending.message, accepted)
                    .await?;
            } else if let Some(accepted) = self.hub()?.peer_turn_receipt(&pending.message).await? {
                self.append_record(
                    &paths.mailbox,
                    MailboxRecord::Accepted {
                        msg_id: pending.message.msg_id.clone(),
                        accepted: accepted.clone(),
                    },
                )
                .await?;
                let _ = self
                    .finish_accepted_turn(&paths.mailbox, &pending.message, accepted)
                    .await?;
            } else {
                let receipt = receipt(&pending.message.msg_id, PeerDelivery::Expired, Some(reason));
                self.append_record(
                    &paths.mailbox,
                    MailboxRecord::Terminal {
                        receipt: receipt.clone(),
                    },
                )
                .await?;
                self.retry_terminal(&paths.mailbox, &pending.message, receipt, false)
                    .await;
            }
        }
        Ok(())
    }

    async fn append_record(&self, path: &Path, record: MailboxRecord) -> Result<(), PeerError> {
        let _serial = self.mailbox_serial.lock().await;
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || append_record_blocking(&path, &record))
            .await
            .map_err(|error| PeerError::Unavailable {
                message: format!("peer mailbox writer task failed: {error}"),
            })?
    }

    async fn lock_mailbox(&self, path: &Path) -> Result<MailboxLease, PeerError> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || lock_mailbox_blocking(&path))
            .await
            .map_err(|error| PeerError::Unavailable {
                message: format!("peer mailbox lock task failed: {error}"),
            })?
    }

    async fn record_sender_receipt(
        &self,
        sender_id: &str,
        receipt: PeerReceipt,
    ) -> Result<(), PeerError> {
        let paths = peer_endpoint_paths(&self.runtime_dir, sender_id, PeerEndpointKind::Haider)?;
        self.append_record(&paths.mailbox, MailboxRecord::Receipt { receipt })
            .await
    }

    #[cfg(unix)]
    async fn record_outbound(
        &self,
        sender_id: &str,
        msg_id: &str,
        target_id: &str,
        target_kind: PeerKind,
        expires_at: u64,
    ) -> Result<(), PeerError> {
        let paths = peer_endpoint_paths(&self.runtime_dir, sender_id, PeerEndpointKind::Haider)?;
        self.append_record(
            &paths.mailbox,
            MailboxRecord::Outbound {
                msg_id: msg_id.to_owned(),
                target_id: target_id.to_owned(),
                target_kind,
                expires_at,
            },
        )
        .await
    }

    #[cfg(unix)]
    async fn accept_wire_receipt(
        &self,
        sender_id: &str,
        receipt: PeerReceipt,
    ) -> Result<(), PeerError> {
        let paths = peer_endpoint_paths(&self.runtime_dir, sender_id, PeerEndpointKind::Haider)?;
        let mailbox = paths.mailbox;
        let _serial = self.mailbox_serial.lock().await;
        tokio::task::spawn_blocking(move || journal_wire_receipt_blocking(&mailbox, receipt))
            .await
            .map_err(|error| PeerError::Unavailable {
                message: format!("peer receipt writer task failed: {error}"),
            })?
    }

    async fn load_pending(
        &self,
        path: &Path,
    ) -> Result<HashMap<String, PendingMessage>, PeerError> {
        let _serial = self.mailbox_serial.lock().await;
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || load_pending_observing_blocking(&path))
            .await
            .map_err(|error| PeerError::Unavailable {
                message: format!("peer mailbox reader task failed: {error}"),
            })?
    }

    async fn load_pending_repairing(
        &self,
        path: &Path,
    ) -> Result<HashMap<String, PendingMessage>, PeerError> {
        let _serial = self.mailbox_serial.lock().await;
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || load_pending_blocking(&path))
            .await
            .map_err(|error| PeerError::Unavailable {
                message: format!("peer mailbox repair task failed: {error}"),
            })?
    }

    #[cfg(unix)]
    async fn expire_outbound(&self, sender_id: &str, mailbox: &Path) -> Result<(), PeerError> {
        let path = mailbox.to_path_buf();
        let now = now_ms();
        let receipts = {
            let _serial = self.mailbox_serial.lock().await;
            tokio::task::spawn_blocking(move || expire_outbound_blocking(&path, now))
                .await
                .map_err(|error| PeerError::Unavailable {
                    message: format!("peer outbound expiry task failed: {error}"),
                })??
        };
        for receipt in receipts {
            if let Ok(hub) = self.hub() {
                hub.publish_peer_event(
                    &SessionId::new(sender_id.to_owned()),
                    WireFrame::PeerDeliveryChanged { receipt },
                );
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    async fn expire_outbound(&self, _sender_id: &str, _mailbox: &Path) -> Result<(), PeerError> {
        Ok(())
    }

    #[cfg(unix)]
    async fn receive_wire(
        self: &Arc<Self>,
        target_id: &str,
        frame: PeerWireFrame,
    ) -> Result<Option<PeerWireFrame>, PeerError> {
        if frame.v != PEER_WIRE_VERSION {
            return Err(PeerError::Invalid {
                message: format!("unsupported peer wire version {}", frame.v),
            });
        }
        match frame.body {
            PeerWireBody::Deliver { mut message } => {
                let msg_id = message.msg_id.clone();
                if message.to != target_id {
                    return Ok(Some(PeerWireFrame::receipt(receipt(
                        &msg_id,
                        PeerDelivery::Refused,
                        Some(PeerDeliveryReason::InvalidMessage),
                    ))));
                }
                // Same-UID proves the OS account, not the claimed identity.
                // Bind attribution to the canonical live manifest. External
                // px registrations are always untrusted; remote ph peers are
                // Haider sessions, while a socket may never claim a session
                // that this daemon owns in process.
                message.from = self.registered_wire_sender(&message.from.id).await?;
                let receipt = match self.enqueue_local(message).await {
                    Ok(receipt) => receipt,
                    Err(PeerError::Invalid { .. }) => receipt(
                        &msg_id,
                        PeerDelivery::Refused,
                        Some(PeerDeliveryReason::InvalidMessage),
                    ),
                    Err(error) => return Err(error),
                };
                Ok(Some(PeerWireFrame::receipt(receipt)))
            }
            PeerWireBody::Receipt { receipt } => {
                validate_receipt(&receipt)?;
                self.accept_wire_receipt(target_id, receipt.clone()).await?;
                if let Ok(hub) = self.hub() {
                    hub.publish_peer_event(
                        &SessionId::new(target_id.to_owned()),
                        WireFrame::PeerDeliveryChanged {
                            receipt: receipt.clone(),
                        },
                    );
                }
                // This echo acknowledges that the sender durably journaled
                // the terminal receipt; the target retries until it sees it.
                Ok(Some(PeerWireFrame::receipt(receipt)))
            }
        }
    }
}

impl Drop for PeerService {
    fn drop(&mut self) {
        self.draining.store(true, Ordering::Release);
        self.wake.notify_waiters();
        if let Ok(task) = self.background.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
        #[cfg(unix)]
        if let Ok(publications) = self.publications.get_mut() {
            for publication in publications.values() {
                let _ = publication.cancel.send(true);
                publication.task.abort();
            }
        }
    }
}

fn descriptor_from_summary(summary: SessionSummary, now: u64) -> PeerDescriptor {
    let workspace = sanitize_peer_scalar(
        &summary.workspace_cwd.unwrap_or_default(),
        MANIFEST_SCALAR_MAX_BYTES,
    );
    let model = sanitize_peer_scalar(
        &summary
            .metadata
            .as_ref()
            .map(|metadata| metadata.model.clone())
            .or(summary.last_model)
            .unwrap_or_default(),
        MANIFEST_SCALAR_MAX_BYTES,
    );
    let started_at = summary
        .metadata
        .as_ref()
        .map_or(0, |metadata| metadata.created_at_ms);
    let id = summary.session_id.to_string();
    let suffix = peer_name_suffix(&id);
    let name = summary
        .title
        .map(|title| sanitize_peer_name(&title))
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| {
            let workspace_name = sanitize_peer_name(&workspace_basename(&workspace));
            let base = if workspace_name.trim().is_empty() {
                "agent"
            } else {
                workspace_name.trim()
            };
            let base = truncate_utf8(base, PEER_NAME_MAX_BYTES - 7);
            format!("{base}-{suffix}")
        });
    let state = match summary.run_state {
        None
        | Some(ObserveRunStateWire::Idle)
        | Some(ObserveRunStateWire::Errored)
        | Some(ObserveRunStateWire::Cancelled) => PeerState::Idle,
        Some(_) => PeerState::Busy,
    };
    PeerDescriptor {
        id,
        name,
        kind: PeerKind::HaiderSession,
        workspace,
        model,
        state,
        started_at,
        last_seen: now,
    }
}

pub(super) fn peer_name_suffix(id: &str) -> String {
    let digest = blake3::hash(id.as_bytes()).to_hex();
    digest.as_str().chars().take(6).collect()
}

fn sanitize_peer_name(value: &str) -> String {
    sanitize_peer_scalar(value.trim(), PEER_NAME_MAX_BYTES)
}

fn sanitize_peer_scalar(value: &str, max_bytes: usize) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    truncate_utf8(&sanitized, max_bytes).to_owned()
}

#[cfg(unix)]
pub(super) fn wire_sender_from_descriptor(descriptor: PeerDescriptor) -> PeerSender {
    PeerSender {
        id: descriptor.id,
        name: descriptor.name,
        kind: descriptor.kind,
        // A same-UID local socket authenticates the OS account, not the
        // manifest publisher. Only the in-process send path is verified.
        trust: PeerTrust::UntrustedExternal,
    }
}

fn descriptor_changed(left: &PeerDescriptor, right: &PeerDescriptor) -> bool {
    left.id != right.id
        || left.name != right.name
        || left.kind != right.kind
        || left.workspace != right.workspace
        || left.model != right.model
        || left.state != right.state
        || left.started_at != right.started_at
}

fn workspace_basename(workspace: &str) -> String {
    Path::new(workspace)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("agent")
        .to_owned()
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &value[..end]
}

pub(super) fn resolve_address(
    address: &str,
    agents: &[PeerDescriptor],
) -> Result<PeerDescriptor, PeerError> {
    if let Some(agent) = agents.iter().find(|agent| agent.id == address) {
        return Ok(agent.clone());
    }
    let (name, prefix) = parse_qualified_address(address);
    let matches = agents
        .iter()
        .filter(|agent| agent.name == name)
        .filter(|agent| prefix.is_none_or(|prefix| agent.id.starts_with(prefix)))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [agent] => Ok((*agent).clone()),
        [] => Err(PeerError::Unavailable {
            message: format!("no live peer matches {address:?}"),
        }),
        _ => Err(PeerError::Ambiguous {
            candidates: matches
                .into_iter()
                .map(|agent| PeerCandidate {
                    id: agent.id.clone(),
                    name: agent.name.clone(),
                })
                .collect(),
        }),
    }
}

pub(super) fn parse_qualified_address(address: &str) -> (&str, Option<&str>) {
    let Some(open) = address.rfind(" [") else {
        return (address, None);
    };
    let Some(prefix) = address.get(open + 2..address.len().saturating_sub(1)) else {
        return (address, None);
    };
    if !address.ends_with(']') || prefix.is_empty() {
        return (address, None);
    }
    (&address[..open], Some(prefix))
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), PeerError> {
    let length = value.len();
    if (!allow_empty && value.trim().is_empty()) || length > max_bytes || value.contains('\0') {
        return Err(PeerError::Invalid {
            message: format!("{field} is {length} bytes; limit is {max_bytes} bytes"),
        });
    }
    Ok(())
}

fn normalize_incoming_message(message: &mut PeerMessage) -> Result<(), PeerError> {
    validate_text(
        "peer message id",
        &message.msg_id,
        PEER_MSG_ID_MAX_BYTES,
        false,
    )?;
    validate_text("peer sender id", &message.from.id, PEER_ID_MAX_BYTES, false)?;
    validate_text(
        "peer sender name",
        &message.from.name,
        PEER_NAME_MAX_BYTES,
        false,
    )?;
    validate_text("peer target id", &message.to, PEER_ID_MAX_BYTES, false)?;
    validate_text(
        "peer message",
        &message.message,
        PEER_MESSAGE_MAX_BYTES,
        false,
    )?;
    if let Some(summary) = message.summary.as_deref() {
        validate_text("peer summary", summary, PEER_SUMMARY_MAX_BYTES, true)?;
    }
    for (field, value) in [
        ("peer message id", message.msg_id.as_str()),
        ("peer sender id", message.from.id.as_str()),
        ("peer sender name", message.from.name.as_str()),
        ("peer target id", message.to.as_str()),
    ] {
        validate_header(field, value)?;
    }
    let queued_at = now_ms();
    // Preserve a normal sender deadline exactly so the target can never
    // deliver after the sender has already expired the same message. A
    // future-skewed/unbounded external deadline is shortened, never extended.
    message.expires_at = message
        .expires_at
        .min(queued_at.saturating_add(MESSAGE_TTL_MS));
    message.queued_at = queued_at;
    Ok(())
}

fn same_delivery(left: &PeerMessage, right: &PeerMessage) -> bool {
    left.msg_id == right.msg_id
        && left.from == right.from
        && left.to == right.to
        && left.message == right.message
        && left.summary == right.summary
}

fn validate_header(field: &'static str, value: &str) -> Result<(), PeerError> {
    if value.chars().any(char::is_control) {
        Err(PeerError::Invalid {
            message: format!("{field} contains a control character"),
        })
    } else {
        Ok(())
    }
}

fn receipt(
    msg_id: &str,
    delivery: PeerDelivery,
    reason: Option<PeerDeliveryReason>,
) -> PeerReceipt {
    PeerReceipt {
        msg_id: msg_id.to_owned(),
        delivery,
        reason,
    }
}

#[cfg(unix)]
fn validate_receipt(receipt: &PeerReceipt) -> Result<(), PeerError> {
    validate_text(
        "peer receipt message id",
        &receipt.msg_id,
        PEER_MSG_ID_MAX_BYTES,
        false,
    )?;
    validate_header("peer receipt message id", &receipt.msg_id)?;
    let reason_is_valid = match receipt.delivery {
        PeerDelivery::Queued | PeerDelivery::Delivered => receipt.reason.is_none(),
        PeerDelivery::Expired | PeerDelivery::Refused => receipt.reason.is_some(),
    };
    if !reason_is_valid {
        return Err(PeerError::Invalid {
            message: format!(
                "peer receipt {} has an invalid reason for {:?}",
                receipt.msg_id, receipt.delivery
            ),
        });
    }
    Ok(())
}

pub(super) fn expiration_receipt(message: &PeerMessage, now: u64) -> Option<PeerReceipt> {
    if now < message.expires_at {
        None
    } else {
        Some(receipt(
            &message.msg_id,
            PeerDelivery::Expired,
            Some(PeerDeliveryReason::TargetNeverReturned),
        ))
    }
}

fn random_id(prefix: &str) -> Result<String, PeerError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| PeerError::Unavailable {
        message: format!("cannot generate peer message id: {error}"),
    })?;
    let mut id = String::with_capacity(prefix.len() + 1 + bytes.len() * 2);
    id.push_str(prefix);
    id.push('-');
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").map_err(|error| PeerError::Unavailable {
            message: format!("cannot format peer message id: {error}"),
        })?;
    }
    Ok(id)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn lock_mailbox_blocking(path: &Path) -> Result<MailboxLease, PeerError> {
    ensure_peer_artifact_parent(path)?;
    lock_mailbox_platform_blocking(path)
}

#[cfg(unix)]
fn lock_mailbox_platform_blocking(path: &Path) -> Result<MailboxLease, PeerError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = OpenOptions::new();
    options
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let file = options
        .open(path)
        .map_err(|error| PeerError::io("open peer mailbox lock", path, error))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|error| PeerError::io("lock peer mailbox", path, error.into()))?;
    Ok(MailboxLease { _file: file })
}

#[cfg(windows)]
fn lock_mailbox_platform_blocking(_path: &Path) -> Result<MailboxLease, PeerError> {
    Ok(MailboxLease)
}

pub(super) fn append_record_blocking(path: &Path, record: &MailboxRecord) -> Result<(), PeerError> {
    ensure_peer_artifact_parent(path)?;
    let mut bytes = serde_json::to_vec(record).map_err(|error| PeerError::Invalid {
        message: format!("cannot encode peer mailbox record: {error}"),
    })?;
    bytes.push(b'\n');
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let mut file = options
        .open(path)
        .map_err(|error| PeerError::io("open peer mailbox", path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| PeerError::io("secure peer mailbox", path, error))?;
    }
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| PeerError::io("append durable peer mailbox", path, error))?;
    // The first queue append creates the directory entry. Syncing only the
    // file is not sufficient to promise survival across a host crash.
    sync_parent(path)
}

pub(super) fn load_pending_blocking(
    path: &Path,
) -> Result<HashMap<String, PendingMessage>, PeerError> {
    load_pending_blocking_impl(path, true)
}

fn load_pending_observing_blocking(
    path: &Path,
) -> Result<HashMap<String, PendingMessage>, PeerError> {
    load_pending_blocking_impl(path, false)
}

fn load_pending_blocking_impl(
    path: &Path,
    repair_torn_suffix: bool,
) -> Result<HashMap<String, PendingMessage>, PeerError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(PeerError::io("open peer mailbox", path, error)),
    };
    let mut pending = HashMap::<String, PendingMessage>::new();
    // Each append ends with LF and syncs. A process crash can leave only the
    // final append torn; ignore that unterminated suffix while treating every
    // malformed completed record as corruption.
    let complete_len = if bytes.ends_with(b"\n") {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1)
    };
    if repair_torn_suffix && complete_len < bytes.len() {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|error| PeerError::io("open torn peer mailbox", path, error))?;
        let complete_len = u64::try_from(complete_len).map_err(|_| PeerError::Invalid {
            message: format!("peer mailbox {} is too large to repair", path.display()),
        })?;
        file.set_len(complete_len)
            .and_then(|()| file.sync_all())
            .map_err(|error| PeerError::io("truncate torn peer mailbox", path, error))?;
    }
    for line in bytes[..complete_len].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let record =
            serde_json::from_slice::<MailboxRecord>(line).map_err(|error| PeerError::Invalid {
                message: format!("invalid peer mailbox record in {}: {error}", path.display()),
            })?;
        match record {
            MailboxRecord::Queued { message } => {
                pending.insert(
                    message.msg_id.clone(),
                    PendingMessage {
                        message,
                        claimed: false,
                        accepted: None,
                        terminal: None,
                        target_published: false,
                        published: false,
                    },
                );
            }
            MailboxRecord::Claimed { msg_id } => {
                if let Some(message) = pending.get_mut(&msg_id) {
                    message.claimed = true;
                }
            }
            MailboxRecord::Accepted { msg_id, accepted } => {
                if let Some(message) = pending.get_mut(&msg_id) {
                    message.accepted = Some(accepted);
                }
            }
            MailboxRecord::Terminal { receipt } => {
                if let Some(message) = pending.get_mut(&receipt.msg_id) {
                    message.terminal = Some(receipt);
                }
            }
            MailboxRecord::TargetPublished { msg_id } => {
                if let Some(message) = pending.get_mut(&msg_id) {
                    message.target_published = true;
                }
            }
            MailboxRecord::Published { msg_id } => {
                if let Some(message) = pending.get_mut(&msg_id) {
                    message.published = true;
                }
            }
            MailboxRecord::Receipt { receipt } => {
                if receipt.msg_id.is_empty() {
                    return Err(PeerError::Invalid {
                        message: format!(
                            "peer mailbox {} contains an empty receipt message id",
                            path.display()
                        ),
                    });
                }
            }
            #[cfg(unix)]
            MailboxRecord::Outbound {
                msg_id,
                target_id,
                expires_at,
                ..
            } => {
                if msg_id.is_empty() || target_id.is_empty() || expires_at == 0 {
                    return Err(PeerError::Invalid {
                        message: format!(
                            "peer mailbox {} contains an invalid outbound expectation",
                            path.display()
                        ),
                    });
                }
            }
        }
    }
    Ok(pending)
}

#[cfg(unix)]
pub(super) fn load_outbound_receipts_blocking(
    path: &Path,
) -> Result<HashMap<String, OutboundReceiptState>, PeerError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(PeerError::io("read peer receipt expectations", path, error)),
    };
    let mut outstanding = HashMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let record =
            serde_json::from_slice::<MailboxRecord>(line).map_err(|error| PeerError::Invalid {
                message: format!("invalid peer mailbox record in {}: {error}", path.display()),
            })?;
        match record {
            MailboxRecord::Outbound {
                msg_id,
                target_id,
                target_kind,
                expires_at,
            } => {
                if msg_id.is_empty() || target_id.is_empty() || expires_at == 0 {
                    return Err(PeerError::Invalid {
                        message: format!(
                            "peer mailbox {} contains an invalid outbound expectation",
                            path.display()
                        ),
                    });
                }
                outstanding.insert(
                    msg_id,
                    OutboundReceiptState::Outstanding {
                        target_kind,
                        expires_at,
                    },
                );
            }
            MailboxRecord::Receipt { receipt } if receipt.delivery != PeerDelivery::Queued => {
                if let Some(state) = outstanding.get_mut(&receipt.msg_id) {
                    *state = OutboundReceiptState::Journaled(receipt);
                }
            }
            _ => {}
        }
    }
    Ok(outstanding)
}

#[cfg(unix)]
pub(super) fn expire_outbound_blocking(
    mailbox: &Path,
    now: u64,
) -> Result<Vec<PeerReceipt>, PeerError> {
    // Repair any torn final append before folding the outbound state. The
    // caller holds the mailbox serializer across this check-and-append, so a
    // concurrent Delivered receipt cannot race an Expired terminal state.
    let _ = load_pending_blocking(mailbox)?;
    let expectations = load_outbound_receipts_blocking(mailbox)?;
    let mut expired = Vec::new();
    for (msg_id, state) in expectations {
        let OutboundReceiptState::Outstanding {
            target_kind: PeerKind::External,
            expires_at,
        } = state
        else {
            continue;
        };
        if now < expires_at {
            continue;
        }
        let receipt = receipt(
            &msg_id,
            PeerDelivery::Expired,
            Some(PeerDeliveryReason::TargetNeverReturned),
        );
        append_record_blocking(
            mailbox,
            &MailboxRecord::Receipt {
                receipt: receipt.clone(),
            },
        )?;
        expired.push(receipt);
    }
    Ok(expired)
}

#[cfg(unix)]
pub(super) fn journal_wire_receipt_blocking(
    mailbox: &Path,
    receipt: PeerReceipt,
) -> Result<(), PeerError> {
    // Loading repairs a torn final append before the correlation scan and
    // preserves strict completed-record validation.
    let _ = load_pending_blocking(mailbox)?;
    let expectations = load_outbound_receipts_blocking(mailbox)?;
    let msg_id = &receipt.msg_id;
    match expectations.get(msg_id) {
        Some(OutboundReceiptState::Outstanding { .. }) => {
            append_record_blocking(mailbox, &MailboxRecord::Receipt { receipt })
        }
        Some(OutboundReceiptState::Journaled(previous)) if previous == &receipt => {
            // The first acknowledgement may have been lost after the journal
            // sync. Echoing an exact retry is idempotent.
            Ok(())
        }
        Some(OutboundReceiptState::Journaled(_)) => Err(PeerError::Invalid {
            message: format!("peer receipt {msg_id:?} conflicts with its durable state"),
        }),
        None => Err(PeerError::Invalid {
            message: format!("peer receipt {msg_id:?} has no outbound delivery"),
        }),
    }
}

fn mailbox_candidates_blocking(runtime_dir: &Path) -> Result<HashSet<PathBuf>, PeerError> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    #[cfg(unix)]
    let owner_uid = rustix::process::geteuid().as_raw();
    let mut candidates = HashSet::new();
    let entries = match std::fs::read_dir(runtime_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidates),
        Err(error) => return Err(PeerError::io("scan peer mailboxes", runtime_dir, error)),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_mailbox_name(name) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        #[cfg(unix)]
        if metadata.uid() != owner_uid || metadata.permissions().mode() & 0o077 != 0 {
            continue;
        }
        candidates.insert(path);
    }
    Ok(candidates)
}

fn is_mailbox_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 17
        && name.starts_with("ph-")
        && name.ends_with(".q")
        && bytes[3..15].iter().all(u8::is_ascii_hexdigit)
}

async fn write_manifest(
    paths: &haider_platform::PeerEndpointPaths,
    descriptor: &PeerDescriptor,
) -> Result<(), PeerError> {
    let socket = paths
        .socket
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PeerError::Invalid {
            message: "peer socket basename is not UTF-8".into(),
        })?
        .to_owned();
    let manifest = PeerManifest {
        version: PEER_WIRE_VERSION,
        id: descriptor.id.clone(),
        name: descriptor.name.clone(),
        kind: descriptor.kind,
        socket,
        capabilities: vec!["deliver".into(), "receipt".into()],
        workspace: descriptor.workspace.clone(),
        model: descriptor.model.clone(),
        state: descriptor.state,
        started_at: descriptor.started_at,
        last_seen: descriptor.last_seen,
    };
    let target = paths.manifest.clone();
    tokio::task::spawn_blocking(move || write_manifest_blocking(&target, &manifest))
        .await
        .map_err(|error| PeerError::Unavailable {
            message: format!("peer manifest writer task failed: {error}"),
        })?
}

fn write_manifest_blocking(path: &Path, manifest: &PeerManifest) -> Result<(), PeerError> {
    ensure_peer_artifact_parent(path)?;
    let bytes = serde_json::to_vec(manifest).map_err(|error| PeerError::Invalid {
        message: format!("cannot encode peer manifest: {error}"),
    })?;
    let temporary = path.with_extension("t");
    haider_platform::validate_runtime_artifact_basename(&temporary)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| PeerError::io("create peer manifest staging file", &temporary, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                PeerError::io("secure peer manifest staging file", &temporary, error)
            })?;
    }
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| PeerError::io("persist peer manifest staging file", &temporary, error))?;
    replace_manifest_staging(&temporary, path)
        .map_err(|error| PeerError::io("publish peer manifest", path, error))?;
    sync_parent(path)
}

fn ensure_peer_artifact_parent(path: &Path) -> Result<(), PeerError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| PeerError::Invalid {
            message: format!("peer artifact {} has no parent directory", path.display()),
        })?;
    std::fs::create_dir_all(parent)
        .map_err(|error| PeerError::io("create peer artifact parent directory", parent, error))
}

#[cfg(unix)]
fn replace_manifest_staging(source: &Path, target: &Path) -> std::io::Result<()> {
    haider_platform::replace_file(source, target)
}

#[cfg(windows)]
fn replace_manifest_staging(source: &Path, target: &Path) -> std::io::Result<()> {
    match haider_platform::replace_file(source, target) {
        Ok(()) => Ok(()),
        // ReplaceFileW requires an existing destination. The initial
        // publication is a same-directory rename; subsequent heartbeats use
        // replacement semantics.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::rename(source, target)
        }
        Err(error) => Err(error),
    }
}

async fn remove_manifest(path: &Path) {
    let path = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
        Ok(()) => sync_parent(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PeerError::io("remove peer manifest", &path, error)),
    })
    .await;
}

fn sync_parent(path: &Path) -> Result<(), PeerError> {
    #[cfg(unix)]
    {
        let parent = path.parent().ok_or_else(|| PeerError::Invalid {
            message: format!("runtime artifact {} has no parent", path.display()),
        })?;
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| PeerError::io("sync peer runtime directory", parent, error))?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
async fn listener_loop(
    mut endpoint: BoundEndpoint,
    service: std::sync::Weak<PeerService>,
    target_id: String,
    mut cancelled: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = cancelled.changed() => {
                if changed.is_err() || *cancelled.borrow() {
                    break;
                }
            }
            accepted = endpoint.accept() => {
                let Ok((mut stream, _)) = accepted else { break };
                let Ok(owner) = haider_platform::peer_is_owner(&stream, endpoint.owner_uid()) else {
                    continue;
                };
                if !owner {
                    continue;
                }
                let Some(service) = service.upgrade() else { break };
                let handled = tokio::time::timeout(WIRE_TIMEOUT, async {
                    let frame = read_frame(&mut stream).await?;
                    if let Some(reply) = service.receive_wire(&target_id, frame).await? {
                        write_frame(&mut stream, &reply).await?;
                    }
                    Ok::<(), PeerError>(())
                }).await;
                if let Ok(Err(error)) = handled {
                    tracing::debug!(target: "haider.peer", %error, "peer socket frame refused");
                }
            }
        }
    }
    endpoint.close_listener();
    let _ = endpoint.cleanup();
}

#[cfg(unix)]
pub(super) async fn discover_unix(runtime_dir: &Path) -> Result<Vec<PeerDescriptor>, PeerError> {
    let directory = runtime_dir.to_path_buf();
    let manifests = tokio::task::spawn_blocking(move || manifest_candidates(&directory))
        .await
        .map_err(|error| PeerError::Unavailable {
            message: format!("peer manifest scan task failed: {error}"),
        })??;
    let mut agents = Vec::new();
    for (manifest, socket) in manifests {
        if tokio::time::timeout(WIRE_TIMEOUT, tokio::net::UnixStream::connect(&socket))
            .await
            .is_ok_and(|result| result.is_ok())
        {
            agents.push(PeerDescriptor {
                id: manifest.id,
                name: manifest.name,
                kind: manifest.kind,
                workspace: manifest.workspace,
                model: manifest.model,
                state: manifest.state,
                started_at: manifest.started_at,
                last_seen: manifest.last_seen,
            });
        }
    }
    Ok(deduplicate_agents(agents))
}

pub(super) fn deduplicate_agents(agents: Vec<PeerDescriptor>) -> Vec<PeerDescriptor> {
    let mut by_id = HashMap::<String, PeerDescriptor>::new();
    for agent in agents {
        match by_id.entry(agent.id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(agent);
            }
            std::collections::hash_map::Entry::Occupied(mut entry)
                if entry.get().kind == PeerKind::External
                    && agent.kind == PeerKind::HaiderSession =>
            {
                entry.insert(agent);
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }
    let mut agents = by_id.into_values().collect::<Vec<_>>();
    agents.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    agents
}

#[cfg(unix)]
fn manifest_candidates(runtime_dir: &Path) -> Result<Vec<(PeerManifest, PathBuf)>, PeerError> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let owner_uid = rustix::process::geteuid().as_raw();
    let entries = std::fs::read_dir(runtime_dir)
        .map_err(|error| PeerError::io("scan peer manifests", runtime_dir, error))?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_manifest_name(name) {
            continue;
        }
        let path = entry.path();
        let Ok(mut file) = rustix::fs::open(
            &path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(std::fs::File::from) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if !metadata.file_type().is_file()
            || metadata.uid() != owner_uid
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() > MANIFEST_MAX_BYTES
        {
            continue;
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        if (&mut file)
            .take(MANIFEST_MAX_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        if bytes.len() as u64 > MANIFEST_MAX_BYTES {
            continue;
        }
        let Ok(manifest) = serde_json::from_slice::<PeerManifest>(&bytes) else {
            continue;
        };
        if manifest.version != PEER_WIRE_VERSION
            || manifest.id.is_empty()
            || manifest.id.len() > PEER_ID_MAX_BYTES
            || manifest.name.is_empty()
            || manifest.name.len() > PEER_NAME_MAX_BYTES
            || !manifest_scalar_safe(&manifest.id, PEER_ID_MAX_BYTES)
            || !manifest_scalar_safe(&manifest.name, PEER_NAME_MAX_BYTES)
            || !manifest_scalar_safe(&manifest.workspace, MANIFEST_SCALAR_MAX_BYTES)
            || !manifest_scalar_safe(&manifest.model, MANIFEST_SCALAR_MAX_BYTES)
            || manifest.socket.contains('/')
            || manifest.socket.contains('\\')
        {
            continue;
        }
        let expected_kind = if name.starts_with("ph-") {
            PeerKind::HaiderSession
        } else {
            PeerKind::External
        };
        if manifest.kind != expected_kind {
            continue;
        }
        let kind = match expected_kind {
            PeerKind::HaiderSession => PeerEndpointKind::Haider,
            PeerKind::External => PeerEndpointKind::External,
        };
        let Ok(paths) = peer_endpoint_paths(runtime_dir, &manifest.id, kind) else {
            continue;
        };
        if paths.manifest != path
            || paths.socket.file_name().and_then(|value| value.to_str())
                != Some(manifest.socket.as_str())
        {
            continue;
        }
        candidates.push((manifest, paths.socket));
    }
    Ok(candidates)
}

#[cfg(unix)]
fn manifest_scalar_safe(value: &str, max_bytes: usize) -> bool {
    value.len() <= max_bytes && !value.chars().any(char::is_control)
}

#[cfg(unix)]
fn is_manifest_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 17
        && name.ends_with(".j")
        && (name.starts_with("ph-") || name.starts_with("px-"))
        && bytes[3..15].iter().all(u8::is_ascii_hexdigit)
}

#[cfg(unix)]
fn endpoint_path_for(
    runtime_dir: &Path,
    descriptor: &PeerDescriptor,
) -> Result<PathBuf, PeerError> {
    let kind = match descriptor.kind {
        PeerKind::HaiderSession => PeerEndpointKind::Haider,
        PeerKind::External => PeerEndpointKind::External,
    };
    Ok(peer_endpoint_paths(runtime_dir, &descriptor.id, kind)?.socket)
}

#[cfg(unix)]
pub(super) async fn exchange_delivery(
    path: &Path,
    frame: PeerWireFrame,
) -> Result<PeerReceipt, PeerError> {
    let expected_msg_id = match &frame.body {
        PeerWireBody::Deliver { message } => message.msg_id.clone(),
        PeerWireBody::Receipt { .. } => {
            return Err(PeerError::Invalid {
                message: "peer delivery exchange requires a delivery frame".into(),
            });
        }
    };
    let result = tokio::time::timeout(WIRE_TIMEOUT, async {
        let mut stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|error| PeerError::io("connect peer socket", path, error))?;
        write_frame(&mut stream, &frame).await?;
        let reply = read_frame(&mut stream).await?;
        if reply.v != PEER_WIRE_VERSION {
            return Err(PeerError::Invalid {
                message: format!("unsupported peer wire version {}", reply.v),
            });
        }
        match reply.body {
            PeerWireBody::Receipt { receipt } => {
                validate_receipt(&receipt)?;
                if receipt.msg_id != expected_msg_id {
                    return Err(PeerError::Invalid {
                        message: format!(
                            "peer receipt message id {:?} does not match {:?}",
                            receipt.msg_id, expected_msg_id
                        ),
                    });
                }
                Ok(receipt)
            }
            PeerWireBody::Deliver { .. } => Err(PeerError::Invalid {
                message: "peer target replied with a delivery frame".into(),
            }),
        }
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => Err(PeerError::Unavailable {
            message: format!(
                "peer delivery to {} exceeded the wire deadline",
                path.display()
            ),
        }),
    }
}

#[cfg(unix)]
pub(super) async fn send_receipt(path: &Path, receipt: PeerReceipt) -> Result<(), PeerError> {
    validate_receipt(&receipt)?;
    let expected = receipt.clone();
    tokio::time::timeout(WIRE_TIMEOUT, async {
        let mut stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|error| PeerError::io("connect peer receipt socket", path, error))?;
        write_frame(&mut stream, &PeerWireFrame::receipt(receipt)).await?;
        let acknowledgement = read_frame(&mut stream).await?;
        if acknowledgement.v != PEER_WIRE_VERSION {
            return Err(PeerError::Invalid {
                message: format!("unsupported peer wire version {}", acknowledgement.v),
            });
        }
        match acknowledgement.body {
            PeerWireBody::Receipt { receipt } if receipt == expected => Ok(()),
            PeerWireBody::Receipt { .. } => Err(PeerError::Invalid {
                message: "peer receipt acknowledgement does not match".into(),
            }),
            PeerWireBody::Deliver { .. } => Err(PeerError::Invalid {
                message: "peer receipt acknowledgement is a delivery frame".into(),
            }),
        }
    })
    .await
    .map_err(|_| PeerError::Unavailable {
        message: format!(
            "peer receipt to {} exceeded the wire deadline",
            path.display()
        ),
    })?
}

#[cfg(unix)]
pub(super) async fn read_frame<R>(reader: &mut R) -> Result<PeerWireFrame, PeerError>
where
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32().await.map_err(|error| PeerError::Io {
        operation: "read peer frame length",
        path: PathBuf::from("<peer socket>"),
        source: error,
    })? as usize;
    if length == 0 || length > PEER_FRAME_MAX_BYTES {
        return Err(PeerError::Invalid {
            message: format!("peer frame is {length} bytes; limit is {PEER_FRAME_MAX_BYTES}"),
        });
    }
    let mut bytes = vec![0_u8; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|error| PeerError::Io {
            operation: "read peer frame body",
            path: PathBuf::from("<peer socket>"),
            source: error,
        })?;
    serde_json::from_slice(&bytes).map_err(|error| PeerError::Invalid {
        message: format!("invalid peer JSON frame: {error}"),
    })
}

#[cfg(unix)]
pub(super) async fn write_frame<W>(writer: &mut W, frame: &PeerWireFrame) -> Result<(), PeerError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(frame).map_err(|error| PeerError::Invalid {
        message: format!("cannot encode peer JSON frame: {error}"),
    })?;
    if bytes.len() > PEER_FRAME_MAX_BYTES {
        return Err(PeerError::Invalid {
            message: format!(
                "encoded peer frame is {} bytes; limit is {PEER_FRAME_MAX_BYTES}",
                bytes.len()
            ),
        });
    }
    let length = u32::try_from(bytes.len()).map_err(|_| PeerError::Invalid {
        message: "peer frame length does not fit the v1 prefix".into(),
    })?;
    writer
        .write_u32(length)
        .await
        .map_err(|error| PeerError::Io {
            operation: "write peer frame length",
            path: PathBuf::from("<peer socket>"),
            source: error,
        })?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|error| PeerError::Io {
            operation: "write peer frame body",
            path: PathBuf::from("<peer socket>"),
            source: error,
        })?;
    writer.flush().await.map_err(|error| PeerError::Io {
        operation: "flush peer frame",
        path: PathBuf::from("<peer socket>"),
        source: error,
    })
}
