//! Registered peer discovery and journaled, idempotent boundary delivery.

mod delivery;

use crate::session_hub::{SessionHub, SessionHubError, WeakSessionHub};
use haider_protocol::ids::SessionId;
#[cfg(unix)]
use haider_protocol::peer::PEER_FRAME_MAX_BYTES;
use haider_protocol::peer::{
    PEER_ID_MAX_BYTES, PEER_MESSAGE_MAX_BYTES, PEER_MSG_ID_MAX_BYTES, PEER_NAME_MAX_BYTES,
    PEER_SUMMARY_MAX_BYTES, PEER_WIRE_VERSION, PeerCandidate, PeerDelivery, PeerDeliveryReason,
    PeerDescriptor, PeerKind, PeerManifest, PeerMessage, PeerReceipt, PeerSender, PeerState,
    PeerTrust,
};
use haider_rpc::{ObserveRunStateWire, SessionSummary, WireFrame};
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

// "session:" + full session id + "@" + full device id; this also
// exceeds the legacy handle + " [" + id-prefix + "]" address ceiling.
const PEER_ADDRESS_MAX_BYTES: usize = 8 + PEER_ID_MAX_BYTES + 1 + PEER_ID_MAX_BYTES;
const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(500);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const RECONCILE_AUDIT_INTERVAL: Duration = Duration::from_secs(30);
const MANIFEST_HEARTBEAT_MS: u64 = 5_000;
pub(super) const MANIFEST_CREATION_SYNC_POLICY: haider_platform::SyncPolicy =
    haider_platform::SyncPolicy::Full;
pub(super) const MANIFEST_HEARTBEAT_SYNC_POLICY: haider_platform::SyncPolicy =
    haider_platform::SyncPolicy::Plain;
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
    Refused {
        message: String,
    },
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
            Self::Ambiguous { candidates } => write!(
                formatter,
                "peer address is ambiguous; candidates: {}",
                candidates
                    .iter()
                    .map(|candidate| format!("{} [{}]", candidate.name, candidate.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Invalid { message }
            | Self::Unavailable { message }
            | Self::Refused { message } => formatter.write_str(message),
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
        match error {
            SessionHubError::Store(error)
                if error.code == haider_protocol::error::ErrorCode::InvalidArgument =>
            {
                Self::Invalid {
                    message: error.to_string(),
                }
            }
            error => Self::Hub(error),
        }
    }
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

const SEND_CAPACITY: usize = 32;

/// One profile daemon's live roster and per-session injection endpoints.
pub(crate) struct PeerService {
    runtime_dir: PathBuf,
    hub: WeakSessionHub,
    draining: AtomicBool,
    wake: Notify,
    reconcile_serial: tokio::sync::Mutex<()>,
    admissions: Arc<tokio::sync::Semaphore>,
    publications: Mutex<HashMap<String, LocalPublication>>,
    background: Mutex<Option<JoinHandle<()>>>,
    outbox: tokio::sync::Mutex<delivery::Outbox>,
    sends: Arc<tokio::sync::Semaphore>,
    send_admission: tokio::sync::Mutex<()>,
    recipient_guards: tokio::sync::Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    #[cfg(test)]
    reconcile_count: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    heartbeat_count: std::sync::atomic::AtomicU64,
}

impl PeerService {
    pub(crate) async fn start(
        runtime_dir: PathBuf,
        hub: &SessionHub,
    ) -> Result<Arc<Self>, PeerError> {
        #[cfg(all(unix, not(target_os = "android")))]
        let runtime_dir = std::env::var_os("HAIDER_PEER_RENDEZVOUS_DIR")
            .map(PathBuf::from)
            .unwrap_or(runtime_dir);
        if !runtime_dir.is_absolute() {
            return Err(PeerError::Invalid {
                message: "peer rendezvous must be an absolute owner-private directory".into(),
            });
        }
        let directory = runtime_dir.clone();
        tokio::task::spawn_blocking(move || {
            haider_platform::prepare_runtime_directory(&directory).map(|_| ())
        })
        .await
        .map_err(|error| PeerError::Unavailable {
            message: format!("prepare peer rendezvous failed: {error}"),
        })??;
        let outbox = delivery::recover(hub).await?;
        let service = Arc::new(Self {
            runtime_dir,
            hub: hub.downgrade(),
            draining: AtomicBool::new(false),
            wake: Notify::new(),
            reconcile_serial: tokio::sync::Mutex::new(()),
            admissions: Arc::new(tokio::sync::Semaphore::new(32)),
            publications: Mutex::new(HashMap::new()),
            background: Mutex::new(None),
            outbox: tokio::sync::Mutex::new(outbox),
            sends: Arc::new(tokio::sync::Semaphore::new(SEND_CAPACITY)),
            send_admission: tokio::sync::Mutex::new(()),
            recipient_guards: tokio::sync::Mutex::new(HashMap::new()),
            #[cfg(test)]
            reconcile_count: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            heartbeat_count: std::sync::atomic::AtomicU64::new(0),
        });
        let mut roster_changes = hub.subscribe_peer_reconcile();
        service.reconcile_once().await?;
        let weak = Arc::downgrade(&service);
        let task = tokio::spawn(async move {
            let now = tokio::time::Instant::now();
            let mut heartbeat =
                tokio::time::interval_at(now + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut audit =
                tokio::time::interval_at(now + RECONCILE_AUDIT_INTERVAL, RECONCILE_AUDIT_INTERVAL);
            audit.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut debounce = None;
            loop {
                let Some(service) = weak.upgrade() else {
                    return;
                };
                if service.draining.load(Ordering::Acquire) {
                    return;
                }
                let reconcile = tokio::select! {
                    biased;
                    () = service.wake.notified() => {
                        debounce = None;
                        true
                    },
                    () = async {
                        if let Some(deadline) = debounce.as_mut() {
                            deadline.await;
                        }
                    }, if debounce.is_some() => {
                        debounce = None;
                        true
                    },
                    _ = audit.tick() => {
                        // Repair a publication that was lost before reaching
                        // the broadcast receiver or while it was lagged.
                        debounce = None;
                        true
                    },
                    _ = heartbeat.tick() => false,
                    received = roster_changes.recv() => match received {
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // The roster publication stream is broader than
                            // peer descriptor state: every committed append
                            // publishes. Arm one debounce only after an event
                            // so a quiet daemon has no 500 ms maintenance tick.
                            // Ready deadlines precede this branch so an event
                            // storm cannot starve the debounce or repair audit.
                            debounce.get_or_insert_with(|| {
                                Box::pin(tokio::time::sleep(RECONCILE_DEBOUNCE))
                            });
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                if service.draining.load(Ordering::Acquire) {
                    return;
                }
                let result = if reconcile {
                    service.reconcile_once().await
                } else {
                    // The five-second timer is only a liveness heartbeat over
                    // cached publications. Store-backed reconciliation is
                    // event driven, with the 30-second audit as repair.
                    service.heartbeat_once().await
                };
                if let Err(error) = service.drain_outbox().await {
                    tracing::warn!(target: "haider.peer", %error, "peer outbox drain failed");
                }
                if let Err(error) = result {
                    if reconcile {
                        debounce.get_or_insert_with(|| {
                            Box::pin(tokio::time::sleep(RECONCILE_DEBOUNCE))
                        });
                    }
                    tracing::warn!(target: "haider.peer", %error, "peer maintenance failed");
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
            // Registration survives endpoint shutdown for bounded offline delivery.
        }
        #[cfg(windows)]
        drop(publications);
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
        self.send_with_options(from, to, message, summary, Default::default())
            .await
    }

    /// A one-shot subscription. Subscribe before the authoritative snapshot,
    /// so an idle transition between those two operations cannot be lost.
    pub(crate) async fn notify_when_idle(
        self: &Arc<Self>,
        to: String,
    ) -> Result<PeerDescriptor, PeerError> {
        self.ensure_running()?;
        let mut changes = self.hub()?.subscribe_peer_reconcile();
        let target = resolve_address(&to, &self.list().await?)?;
        if !self.is_local(&target.id)? {
            #[cfg(unix)]
            {
                let path = endpoint_path_for(&self.runtime_dir, &target)?;
                let connected = connect_peer(&path).await?;
                let peers = haider_client::peer_messaging(&connected.client).ok_or_else(|| {
                    PeerError::Unavailable {
                        message: "target lacks peer messaging".into(),
                    }
                })?;
                let result = peers
                    .notify_when_idle(target.address())
                    .await
                    .map_err(|error| PeerError::Unavailable {
                        message: error.to_string(),
                    });
                let _ = connected.client.close();
                return result;
            }
            #[cfg(windows)]
            return Err(PeerError::Unavailable {
                message: "target device transport unavailable".into(),
            });
        }
        loop {
            self.ensure_running()?;
            let agent = self
                .hub()?
                .peer_session_summary(&SessionId::new(target.id.clone()))
                .await?
                .map(|summary| descriptor_from_summary(summary, now_ms(), &target.device_id))
                .ok_or_else(|| PeerError::Unavailable {
                    message: "target is no longer live".into(),
                })?;
            if agent.state == PeerState::Idle {
                return Ok(agent);
            }
            tokio::select! {
                _ = self.wake.notified() => self.ensure_running()?,
                event = async {
                    loop {
                        match changes.recv().await {
                            Ok(id) if id.as_str() != target.id => continue,
                            event => break event,
                        }
                    }
                } => {
                    if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        return Err(PeerError::Unavailable { message: "peer roster closed".into() });
                    }
                }
            }
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
        #[cfg(test)]
        self.reconcile_count.fetch_add(1, Ordering::Relaxed);
        let summaries = self.hub()?.peer_session_summaries().await?;
        let now = now_ms();
        let device_id = self.hub()?.peer_device_id();
        let desired = summaries
            .into_iter()
            .map(|summary| {
                let descriptor = descriptor_from_summary(summary, now, &device_id);
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
                let write = {
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
                        Some((
                            publication.paths.clone(),
                            if changed {
                                MANIFEST_CREATION_SYNC_POLICY
                            } else {
                                MANIFEST_HEARTBEAT_SYNC_POLICY
                            },
                        ))
                    } else {
                        None
                    }
                };
                if let Some((paths, policy)) = write {
                    write_manifest(&paths, descriptor, policy).await?;
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
                #[cfg(windows)]
                drop(publication);
                // Losing residency removes only the endpoint, not registration.
            }
        }
        Ok(())
    }

    async fn heartbeat_once(&self) -> Result<(), PeerError> {
        #[cfg(test)]
        self.heartbeat_count.fetch_add(1, Ordering::Relaxed);
        let now = now_ms();
        let due = {
            let mut publications =
                self.publications
                    .lock()
                    .map_err(|_| PeerError::Unavailable {
                        message: "peer publication registry is poisoned".into(),
                    })?;
            publications
                .values_mut()
                .filter(|publication| {
                    now.saturating_sub(publication.descriptor.last_seen) >= MANIFEST_HEARTBEAT_MS
                })
                .map(|publication| {
                    publication.descriptor.last_seen = now;
                    (publication.paths.clone(), publication.descriptor.clone())
                })
                .collect::<Vec<_>>()
        };
        for (paths, descriptor) in due {
            write_manifest(&paths, &descriptor, MANIFEST_HEARTBEAT_SYNC_POLICY).await?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn active_admissions_for_test(&self) -> usize {
        32 - self.admissions.available_permits()
    }

    #[cfg(test)]
    pub(super) async fn wait_for_admissions_idle_for_test(&self) -> Result<(), PeerError> {
        let _permits =
            self.admissions
                .acquire_many(32)
                .await
                .map_err(|_| PeerError::Unavailable {
                    message: "peer admission budget closed".into(),
                })?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn reconcile_count(&self) -> u64 {
        self.reconcile_count.load(Ordering::Relaxed)
    }

    /// Waits until a reconciliation already observed by [`Self::reconcile_count`]
    /// has left the serialized store/publication section.
    #[cfg(test)]
    pub(super) async fn wait_for_reconcile_idle(&self) {
        let _serial = self.reconcile_serial.lock().await;
    }

    #[cfg(test)]
    pub(super) fn heartbeat_count(&self) -> u64 {
        self.heartbeat_count.load(Ordering::Relaxed)
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
            write_manifest(&paths, &descriptor, MANIFEST_CREATION_SYNC_POLICY).await?;
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
            write_manifest(&paths, &descriptor, MANIFEST_CREATION_SYNC_POLICY).await?;
            Ok(LocalPublication { descriptor, paths })
        }
    }

    async fn discover(&self) -> Result<Vec<PeerDescriptor>, PeerError> {
        #[cfg(unix)]
        {
            // The Unix primary endpoint is scoped entirely by its runtime
            // directory. Keep it out of recurring peer hygiene so discovery
            // never connects the daemon to its own listener.
            let own_endpoint = Endpoint::new(&self.runtime_dir, "");
            let _ = haider_platform::sweep_stale_endpoints(
                &self.runtime_dir,
                Some(own_endpoint.address()),
            )
            .await;
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
        self.ensure_running()?;
        normalize_incoming_message(&mut message)?;
        if !self.is_local(&message.to)? {
            return Err(PeerError::Unavailable {
                message: "target is no longer live".into(),
            });
        }
        let hub = self.hub()?;
        let permit = Arc::clone(&self.admissions)
            .try_acquire_owned()
            .map_err(|_| PeerError::Unavailable {
                message: "peer admission capacity is full".into(),
            })?;
        let admitted_message = message.clone();
        let admitting_hub = hub.clone();
        // Once handed to the actor, acceptance can commit independently of
        // the requester. Own the whole acceptance -> worker handoff so a
        // disconnected sender cannot strand a committed run. The service-
        // wide permit stays with the task, bounding even disconnected callers.
        let accepted = tokio::spawn(async move {
            let _permit = permit;
            admitting_hub.inject_peer_message(&admitted_message).await
        })
        .await
        .map_err(|error| PeerError::Unavailable {
            message: format!("peer admission task failed: {error}"),
        })??;
        // Optional compatibility notification, derived only after transcript
        // admission. It has no publication marker or separate durability.
        hub.publish_peer_event(
            &SessionId::new(message.to.clone()),
            WireFrame::PeerMessageReceived {
                message: message.clone(),
            },
        );
        // Keep legacy busy/idle delivery values, with an additive status
        // confirming durable admission in either case.
        let mut receipt = receipt(
            &message.msg_id,
            match accepted.disposition {
                haider_core::TurnAdmissionDisposition::Started => PeerDelivery::Delivered,
                _ => PeerDelivery::Queued,
            },
            None,
        );
        receipt.status = Some(haider_protocol::peer::PeerReceiptStatus {
            state: haider_protocol::peer::PeerDeliveryState::Delivered,
            from: Some(message.from.address()),
            reason: None,
            to: message.to.clone(),
            accepted_at_ms: message.queued_at,
            updated_at_ms: now_ms(),
        });
        Ok(receipt)
    }

    #[cfg(unix)]
    async fn registered_wire_sender(
        &self,
        id: &str,
        device_id: &str,
    ) -> Result<PeerSender, PeerError> {
        if self.is_local(id)? {
            return Err(PeerError::Invalid {
                message: format!("socket peer cannot claim local Haider session {id:?}"),
            });
        }
        let descriptor = self
            .known_peers()
            .await?
            .into_iter()
            .find(|agent| agent.id == id && agent.device_id == device_id)
            .ok_or_else(|| PeerError::Invalid {
                message: format!("socket peer {id:?} has no registered, unambiguous manifest"),
            })?;
        Ok(wire_sender_from_descriptor(descriptor))
    }

    #[cfg(unix)]
    async fn receive_request(
        self: &Arc<Self>,
        target_id: &str,
        body: haider_rpc::RequestBody,
    ) -> Result<haider_rpc::ResponseBody, PeerError> {
        match body {
            haider_rpc::RequestBody::PeerInject { mut message } => {
                if message.to != target_id {
                    return Err(PeerError::Invalid { message: "wrong peer endpoint target".into() });
                }
                message.from = self.registered_wire_sender(&message.from.id, &message.from.device_id).await?;
                let receipt = self.enqueue_local(message).await?;
                Ok(haider_rpc::ResponseBody::PeerSend { receipt })
            }
            haider_rpc::RequestBody::PeerNotifyWhenIdle { to } => {
                // The endpoint may observe only its own resident session.
                let target = resolve_address(&to, &self.list().await?)?;
                if target.id != target_id {
                    return Err(PeerError::Invalid { message: "wrong idle subscription endpoint".into() });
                }
                let agent = self.notify_when_idle(to).await?;
                Ok(haider_rpc::ResponseBody::PeerNotifyWhenIdle { agent })
            }
            _ => Err(PeerError::Invalid {
                message: "a peer endpoint accepts only injection or an idle subscription; a peer cannot approve anything".into(),
            }),
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

fn descriptor_from_summary(summary: SessionSummary, now: u64, device_id: &str) -> PeerDescriptor {
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
        device_id: device_id.into(),
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
        device_id: descriptor.device_id,
        mode: match descriptor.state {
            PeerState::Busy => "prompting",
            PeerState::Idle => "idle",
        }
        .into(),
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
    if let Some(agent) = agents
        .iter()
        .find(|agent| agent.id == address || agent.address() == address)
    {
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
            message: format!(
                "no registered peer matches {address:?}; candidates: {}",
                agents
                    .iter()
                    .map(|agent| format!("{} ({})", agent.address(), agent.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
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
    let mut has_content = false;
    message
        .message
        .visit_strs(|text| has_content |= !text.trim().is_empty());
    if !has_content || message.message.len() > PEER_MESSAGE_MAX_BYTES {
        return Err(PeerError::Invalid {
            message: "peer message is empty or exceeds the byte limit".into(),
        });
    }
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
    validate_header("peer sender device", &message.from.device_id)?;
    validate_text(
        "peer sender device",
        &message.from.device_id,
        PEER_ID_MAX_BYTES,
        true,
    )?;
    validate_header("peer sender mode", &message.from.mode)?;
    validate_text(
        "peer sender mode",
        &message.from.mode,
        PEER_NAME_MAX_BYTES,
        false,
    )?;
    Ok(())
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
        status: None,
        msg_id: msg_id.to_owned(),
        delivery,
        reason,
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

async fn write_manifest(
    paths: &haider_platform::PeerEndpointPaths,
    descriptor: &PeerDescriptor,
    sync_policy: haider_platform::SyncPolicy,
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
        device_id: descriptor.device_id.clone(),
        name: descriptor.name.clone(),
        kind: descriptor.kind,
        socket,
        capabilities: vec![haider_rpc::FEATURE_PEER_AGENT_INJECTION_V1.into()],
        workspace: descriptor.workspace.clone(),
        model: descriptor.model.clone(),
        state: descriptor.state,
        started_at: descriptor.started_at,
        last_seen: descriptor.last_seen,
    };
    let target = paths.manifest.clone();
    tokio::task::spawn_blocking(move || write_manifest_blocking(&target, &manifest, sync_policy))
        .await
        .map_err(|error| PeerError::Unavailable {
            message: format!("peer manifest writer task failed: {error}"),
        })?
}

pub(super) fn write_manifest_blocking(
    path: &Path,
    manifest: &PeerManifest,
    sync_policy: haider_platform::SyncPolicy,
) -> Result<(), PeerError> {
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
        .and_then(|()| sync_manifest_file(&file, sync_policy))
        .map_err(|error| PeerError::io("persist peer manifest staging file", &temporary, error))?;
    // Windows replacement APIs require the staged file to be closed before
    // publication. Unix retains the same completed-file atomic rename, while
    // Windows no longer asks ReplaceFileW to move our own live handle.
    drop(file);
    replace_manifest_staging(&temporary, path)
        .map_err(|error| PeerError::io("publish peer manifest", path, error))?;
    sync_manifest_parent(path, sync_policy)
}

#[cfg(test)]
type ManifestSyncTestHook = Box<dyn FnMut(haider_platform::SyncPolicy)>;

#[cfg(test)]
std::thread_local! {
    static MANIFEST_SYNC_TEST_HOOK: std::cell::RefCell<Option<ManifestSyncTestHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn with_manifest_sync_test_hook<T>(
    hook: impl FnMut(haider_platform::SyncPolicy) + 'static,
    action: impl FnOnce() -> T,
) -> T {
    let previous = MANIFEST_SYNC_TEST_HOOK.with(|slot| slot.replace(Some(Box::new(hook))));
    let result = action();
    MANIFEST_SYNC_TEST_HOOK.with(|slot| {
        slot.replace(previous);
    });
    result
}

#[cfg(test)]
fn intercept_manifest_sync_for_test(policy: haider_platform::SyncPolicy) -> bool {
    MANIFEST_SYNC_TEST_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(hook) = slot.as_mut() else {
            return false;
        };
        hook(policy);
        true
    })
}

fn sync_manifest_file(
    file: &std::fs::File,
    policy: haider_platform::SyncPolicy,
) -> std::io::Result<()> {
    #[cfg(test)]
    if intercept_manifest_sync_for_test(policy) {
        return Ok(());
    }
    haider_platform::fs::sync_file(file, policy)
}

fn sync_manifest_parent(path: &Path, policy: haider_platform::SyncPolicy) -> Result<(), PeerError> {
    let parent = path.parent().ok_or_else(|| PeerError::Invalid {
        message: format!("runtime artifact {} has no parent", path.display()),
    })?;
    #[cfg(test)]
    if intercept_manifest_sync_for_test(policy) {
        return Ok(());
    }
    haider_platform::fs::sync_directory(parent, policy)
        .map_err(|error| PeerError::io("sync peer runtime directory", parent, error))
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

#[cfg(unix)]
async fn listener_loop(
    mut endpoint: BoundEndpoint,
    service: std::sync::Weak<PeerService>,
    target_id: String,
    mut cancelled: watch::Receiver<bool>,
) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            changed = cancelled.changed() => {
                if changed.is_err() || *cancelled.borrow() { break; }
            }
            _ = connections.join_next(), if !connections.is_empty() => {},
            accepted = endpoint.accept() => {
                let Ok((stream, _)) = accepted else { break };
                if !haider_platform::peer_is_owner(&stream, endpoint.owner_uid()).unwrap_or(false) {
                    continue;
                }
                // Bounded live subscriptions and handshakes, with no sender timers.
                if connections.len() >= 32 { continue; }
                let Some(service) = service.upgrade() else { break };
                let target = target_id.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_peer_connection(stream, service, target).await {
                        tracing::debug!(target: "haider.peer", %error, "peer connection closed");
                    }
                });
            }
        }
    }
    connections.shutdown().await;
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
                device_id: manifest.device_id,
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
            || !manifest_scalar_safe(&manifest.device_id, PEER_ID_MAX_BYTES)
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
async fn connect_peer(path: &Path) -> Result<haider_client::Connected, PeerError> {
    let connected = haider_client::connect(
        path,
        haider_client::ClientConfig {
            client_name: "haider-peer".into(),
            capabilities: Default::default(),
            frame_limit: PEER_FRAME_MAX_BYTES,
            ..Default::default()
        },
    )
    .await
    .map_err(|error| match error {
        haider_client::ConnectError::PermissionDenied(_)
        | haider_client::ConnectError::Rejected(_)
        | haider_client::ConnectError::Frame(_)
        | haider_client::ConnectError::UnexpectedFrame => PeerError::Refused {
            message: error.to_string(),
        },
        _ => PeerError::Unavailable {
            message: error.to_string(),
        },
    })?;
    if !connected
        .welcome
        .features
        .contains(haider_rpc::FEATURE_PEER_AGENT_INJECTION_V1)
    {
        let _ = connected.client.close();
        return Err(PeerError::Refused {
            message: "target lacks peer_agent_injection_v1".into(),
        });
    }
    Ok(connected)
}

#[cfg(unix)]
pub(super) async fn exchange_delivery(
    path: &Path,
    message: PeerMessage,
    device_id: &str,
) -> Result<PeerReceipt, PeerError> {
    let connected = connect_peer(path).await?;
    if connected.welcome.instance_id != message.to || connected.welcome.profile_id != device_id {
        let _ = connected.client.close();
        return Err(PeerError::Refused {
            message: "endpoint Welcome names a different receiver or device".into(),
        });
    }
    let response = connected
        .client
        .request(haider_rpc::RequestBody::PeerInject { message })
        .await;
    let _ = connected.client.close();
    let response = response.map_err(|error| match error {
        haider_client::ClientError::Encode(_)
        | haider_client::ClientError::MissingFeature(_)
        | haider_client::ClientError::Disconnected(
            haider_client::DisconnectReason::Protocol(_)
            | haider_client::DisconnectReason::Fatal(_),
        ) => PeerError::Refused {
            message: error.to_string(),
        },
        _ => PeerError::Unavailable {
            message: error.to_string(),
        },
    })?;
    haider_client::peer::peer_send_response(response).map_err(|error| match error {
        haider_client::PeerClientError::Refused { ref code, .. }
            if code == haider_rpc::ERROR_CODE_PEER_UNAVAILABLE =>
        {
            PeerError::Unavailable {
                message: error.to_string(),
            }
        }
        _ => PeerError::Refused {
            message: error.to_string(),
        },
    })
}

#[cfg(unix)]
type PeerConnectionFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), PeerError>> + Send>>;

#[cfg(unix)]
fn serve_peer_connection(
    mut stream: haider_platform::IpcStream,
    service: Arc<PeerService>,
    target: String,
) -> PeerConnectionFuture {
    Box::pin(async move {
        use haider_rpc::{LifecyclePhase, ResponseBody, Welcome};
        // Reuse the ordinary RPC client's negotiated handshake budget. The
        // subscription itself has no timeout; its reader services every Ping.
        let hello = tokio::time::timeout(
            haider_client::ClientConfig::default().handshake_timeout,
            read_frame(&mut stream),
        )
        .await
        .map_err(|_| PeerError::Unavailable {
            message: "peer handshake deadline elapsed".into(),
        })??;
        let WireFrame::Hello(hello) = hello else {
            return Err(PeerError::Invalid {
                message: "peer connection requires Hello".into(),
            });
        };
        let negotiated = haider_rpc::negotiate(
            &hello,
            &haider_rpc::ServerRange {
                protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
                protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
                capabilities: Default::default(),
                supports_msgpack: false,
            },
        )
        .map_err(|error| PeerError::Invalid {
            message: error.message,
        })?;
        let frame_limit = (hello.max_receive_frame as usize).min(PEER_FRAME_MAX_BYTES);
        let hub = service.hub()?;
        write_frame_limited(
            &mut stream,
            &WireFrame::Welcome(Welcome {
                protocol: negotiated.protocol,
                instance_id: target.clone(),
                daemon_generation: 0,
                frame_limit: frame_limit as u32,
                profile_id: hub.peer_device_id(),
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                lifecycle_phase: LifecyclePhase::Ready,
                capabilities_granted: Default::default(),
                features: [
                    haider_rpc::FEATURE_PEER_AGENT_INJECTION_V1.into(),
                    haider_rpc::FEATURE_PEER_MESSAGING_V1.into(),
                ]
                .into(),
                user_command_withheld: false,
                encoding: None,
            }),
            frame_limit,
        )
        .await?;
        let (request_id, body) = loop {
            match read_frame_limited(&mut stream, frame_limit).await? {
                WireFrame::Ping { nonce } => {
                    write_frame_limited(&mut stream, &WireFrame::Pong { nonce }, frame_limit)
                        .await?
                }
                WireFrame::Request { request_id, body } => break (request_id, body),
                WireFrame::MenuAnswer { request_id, .. } => {
                    let message =
                        "a peer cannot approve anything or launder permissions".to_owned();
                    let frame = match request_id {
                        Some(request_id) => WireFrame::Response {
                            request_id,
                            body: ResponseBody::Error {
                                code: haider_rpc::ERROR_CODE_PEER_INVALID.into(),
                                message,
                                retryable: false,
                                data: None,
                            },
                        },
                        None => WireFrame::ProtocolError(haider_rpc::ProtocolError {
                            code: haider_rpc::ERROR_CODE_PEER_INVALID.into(),
                            message,
                            fatal: true,
                            presentation: None,
                            failed_write_ids: Vec::new(),
                        }),
                    };
                    return write_frame_limited(&mut stream, &frame, frame_limit).await;
                }
                _ => {
                    return Err(PeerError::Invalid {
                        message: "expected peer RPC request".into(),
                    });
                }
            }
        };
        let request = service.receive_request(&target, body);
        tokio::pin!(request);
        // Keep the future intact across keepalive frames. In particular a Ping
        // neither restarts an admission nor changes the target's run deadline.
        let result = loop {
            tokio::select! {
                result = &mut request => break result,
                frame = read_frame_limited(&mut stream, frame_limit) => match frame? {
                    WireFrame::Ping { nonce } => write_frame_limited(&mut stream, &WireFrame::Pong { nonce }, frame_limit).await?,
                    WireFrame::Pong { .. } => {},
                    _ => return Err(PeerError::Invalid { message: "one peer request per connection".into() }),
                }
            }
        };
        let body = result.unwrap_or_else(|error| ResponseBody::Error {
            code: match error {
                PeerError::Invalid { .. } => haider_rpc::ERROR_CODE_PEER_INVALID,
                PeerError::Ambiguous { .. } => haider_rpc::ERROR_CODE_PEER_AMBIGUOUS,
                _ => haider_rpc::ERROR_CODE_PEER_UNAVAILABLE,
            }
            .into(),
            message: error.to_string(),
            retryable: false,
            data: None,
        });
        write_frame_limited(
            &mut stream,
            &WireFrame::Response { request_id, body },
            frame_limit,
        )
        .await?;
        // A one-shot response ends this endpoint connection and drops its watch.
        Ok(())
    })
}

#[cfg(unix)]
pub(super) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<WireFrame, PeerError> {
    read_frame_limited(reader, PEER_FRAME_MAX_BYTES).await
}

#[cfg(unix)]
async fn read_frame_limited<R: AsyncRead + Unpin>(
    reader: &mut R,
    frame_limit: usize,
) -> Result<WireFrame, PeerError> {
    // The shared RPC decoder owns framing, bounds, poison semantics and the
    // protocol union. BytesMut freezes the completed socket body once.
    let length = reader
        .read_u32()
        .await
        .map_err(|error| PeerError::io("read peer RPC length", "<peer>", error))?
        as usize;
    if length == 0 || length > frame_limit {
        return Err(PeerError::Invalid {
            message: "peer RPC frame exceeds negotiated bound".into(),
        });
    }
    let mut bytes = bytes::BytesMut::zeroed(length);
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|error| PeerError::io("read peer RPC body", "<peer>", error))?;
    haider_rpc::uds_codec::decode_owned_json(bytes.freeze(), frame_limit).map_err(|error| {
        PeerError::Invalid {
            message: error.to_string(),
        }
    })
}

#[cfg(unix)]
async fn write_frame_limited<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &WireFrame,
    limit: usize,
) -> Result<(), PeerError> {
    let encoded = haider_rpc::uds_codec::encode_zeroizing_parts_with(
        frame,
        limit,
        haider_rpc::WireEncoding::Json,
    )
    .map_err(|error| PeerError::Invalid {
        message: error.to_string(),
    })?;
    writer
        .write_all(encoded.prefix())
        .await
        .map_err(|error| PeerError::io("write peer RPC prefix", "<peer>", error))?;
    writer
        .write_all(encoded.body())
        .await
        .map_err(|error| PeerError::io("write peer RPC body", "<peer>", error))?;
    writer
        .flush()
        .await
        .map_err(|error| PeerError::io("flush peer RPC", "<peer>", error))
}

#[cfg(all(test, unix))]
#[path = "../peer_delivery_tests.rs"]
mod peer_delivery_tests;
