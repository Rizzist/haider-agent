//! Sender outbox projection. The session journal is the sole authority;
//! this bounded map is rebuilt on boot and never imports retired .q files.

use super::*;
use futures_util::{StreamExt, stream};
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::EventId;
use haider_protocol::peer::{
    PeerDeliveryState, PeerOutboxEntry, PeerReceiptStatus, PeerSendOptions, PeerStatusPage,
    PeerStatusQuery,
};
use std::collections::BTreeMap;

pub(super) const MAX_PER_RECIPIENT: usize = 32;
const MAX_PENDING: usize = 256;
const MAX_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const DEFAULT_TTL_MS: u64 = 60 * 60 * 1000;
const PAGE_SIZE: usize = 128;
type Key = (String, String);

#[derive(Clone)]
pub(super) struct Pending {
    entry: PeerOutboxEntry,
    receipt: PeerReceipt,
    journal_seq: u64,
}
pub(super) type Outbox = BTreeMap<Key, Pending>;

fn order_key<'a>(key: &'a Key, pending: &Pending) -> (Option<u64>, u64, u64, &'a Key) {
    // Pre-ordering journals sort first, using their original timestamp and
    // session sequence with a stable final tie-break. New entries never use
    // wall-clock time to decide their relative order.
    (
        pending.entry.enqueue_order,
        pending.entry.message.queued_at,
        pending.journal_seq,
        key,
    )
}

fn is_pending(receipt: &PeerReceipt) -> bool {
    receipt.status.as_ref().is_some_and(|status| {
        matches!(
            status.state,
            PeerDeliveryState::Accepted
                | PeerDeliveryState::Held
                | PeerDeliveryState::HeldForApproval
        )
    })
}

pub(super) async fn recover(hub: &SessionHub) -> Result<Outbox, PeerError> {
    let mut pending = Outbox::new();
    for session in hub.session_ids().await? {
        let mut after = 0;
        loop {
            let events = hub
                .read_internal_session(&session, after, PAGE_SIZE)
                .await
                .map_err(SessionHubError::from)?;
            let full = events.len() == PAGE_SIZE;
            for event in events {
                after = event.seq;
                match event.payload.decode_event() {
                    Ok(EventPayload::PeerOutbox(entry))
                        if entry.message.from.id == session.as_str()
                            && entry.message.from.device_id == hub.peer_device_id() =>
                    {
                        let receipt = status_receipt(&entry, PeerDeliveryState::Accepted, None);
                        pending.insert(
                            (session.to_string(), entry.message.msg_id.clone()),
                            Pending {
                                entry,
                                receipt,
                                journal_seq: event.seq,
                            },
                        );
                    }
                    Ok(EventPayload::PeerDelivery(receipt)) => {
                        let key = (session.to_string(), receipt.msg_id.clone());
                        if is_pending(&receipt) {
                            if let Some(record) = pending.get_mut(&key) {
                                record.receipt = receipt;
                            }
                        } else {
                            pending.remove(&key);
                        }
                    }
                    _ => {}
                }
            }
            if !full {
                break;
            }
        }
    }
    Ok(pending)
}

fn status_receipt(
    entry: &PeerOutboxEntry,
    state: PeerDeliveryState,
    reason: Option<String>,
) -> PeerReceipt {
    PeerReceipt {
        msg_id: entry.message.msg_id.clone(),
        delivery: match state {
            PeerDeliveryState::Delivered => PeerDelivery::Delivered,
            PeerDeliveryState::Failed => PeerDelivery::Refused,
            _ => PeerDelivery::Queued,
        },
        reason: (state == PeerDeliveryState::Failed).then_some(PeerDeliveryReason::TargetRefused),
        status: Some(PeerReceiptStatus {
            state,
            from: Some(entry.message.from.address()),
            reason: reason.map(|reason| sanitize_peer_scalar(&reason, 2048)),
            to: entry.target.address(),
            accepted_at_ms: entry.message.queued_at,
            updated_at_ms: now_ms(),
        }),
    }
}

async fn journal(
    hub: &SessionHub,
    session: &SessionId,
    payloads: Vec<EventPayload>,
) -> Result<u64, PeerError> {
    let mut envelopes = Vec::with_capacity(payloads.len());
    for payload in payloads {
        envelopes.push(EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(random_id("peer-status")?),
            seq: 0,
            session_id: session.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: hub.device_id(),
            authority_epoch: 0,
            worker_generation: hub.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload)
                .map_err(|error| PeerError::Invalid {
                    message: error.to_string(),
                })?
                .into(),
        });
    }
    hub.append(&mut envelopes)
        .await
        .map_err(SessionHubError::from)?;
    Ok(envelopes.first().map_or(0, |event| event.seq))
}

impl PeerService {
    pub(crate) async fn delivery_status(
        &self,
        query: PeerStatusQuery,
    ) -> Result<PeerStatusPage, PeerError> {
        if let Some(id) = &query.msg_id {
            validate_text("peer message id", id, PEER_MSG_ID_MAX_BYTES, false)?;
        }
        let events = self
            .hub()?
            .read_internal_session(&query.session_id, query.after_seq, PAGE_SIZE)
            .await
            .map_err(SessionHubError::from)?;
        let next_seq = events.last().map_or(query.after_seq, |event| event.seq);
        let has_more = events.len() == PAGE_SIZE;
        let receipts = events
            .into_iter()
            .filter_map(|event| match event.payload.decode_event() {
                Ok(EventPayload::PeerDelivery(receipt))
                    if query.msg_id.as_ref().is_none_or(|id| *id == receipt.msg_id) =>
                {
                    Some(receipt)
                }
                _ => None,
            })
            .collect();
        Ok(PeerStatusPage {
            receipts,
            next_seq,
            has_more,
        })
    }

    async fn prior_send(
        &self,
        from: &SessionId,
        msg_id: &str,
    ) -> Result<Option<Pending>, PeerError> {
        let mut found: Option<Pending> = None;
        let mut after = 0;
        let hub = self.hub()?;
        loop {
            let events = hub
                .read_internal_session(from, after, PAGE_SIZE)
                .await
                .map_err(SessionHubError::from)?;
            let full = events.len() == PAGE_SIZE;
            for event in events {
                after = event.seq;
                match event.payload.decode_event() {
                    Ok(EventPayload::PeerOutbox(entry)) if entry.message.msg_id == msg_id => {
                        let receipt = status_receipt(&entry, PeerDeliveryState::Accepted, None);
                        found = Some(Pending {
                            entry,
                            receipt,
                            journal_seq: event.seq,
                        });
                    }
                    Ok(EventPayload::PeerDelivery(receipt)) if receipt.msg_id == msg_id => {
                        if let Some(record) = found.as_mut() {
                            record.receipt = receipt;
                        }
                    }
                    _ => {}
                }
            }
            if !full {
                return Ok(found);
            }
        }
    }

    /// Once admitted to this bounded service task, a sender disconnect cannot
    /// cancel the journal commit or leave a committed send outside the map.
    pub(crate) async fn send_with_options(
        self: &Arc<Self>,
        from: &SessionId,
        to: String,
        message: String,
        summary: Option<String>,
        options: PeerSendOptions,
    ) -> Result<PeerReceipt, PeerError> {
        self.ensure_running()?;
        let permit =
            self.sends
                .clone()
                .try_acquire_owned()
                .map_err(|_| PeerError::Unavailable {
                    message: "peer send capacity is full".into(),
                })?;
        let service = self.clone();
        let from = from.clone();
        tokio::spawn(async move {
            let result = service
                .send_owned(from.clone(), to, message, summary, options)
                .await;
            drop(permit);
            // Validation failures may produce no journal publication. Release
            // the in-flight retirement guard before waking the idle loop.
            if let Ok(hub) = service.hub() {
                hub.notify_peer_delivery_settled(from);
            }
            result
        })
        .await
        .map_err(|error| PeerError::Unavailable {
            message: format!("peer send task failed: {error}"),
        })?
    }

    async fn send_owned(
        self: &Arc<Self>,
        from: SessionId,
        to: String,
        text: String,
        summary: Option<String>,
        options: PeerSendOptions,
    ) -> Result<PeerReceipt, PeerError> {
        if let Some(id) = &options.msg_id {
            validate_text("peer message id", id, PEER_MSG_ID_MAX_BYTES, false)?;
            validate_header("peer message id", id)?;
        }
        if options.cancel && options.msg_id.is_none() {
            return Err(PeerError::Invalid {
                message: "cancellation requires msg_id".into(),
            });
        }
        // Serialize idempotency lookup and durable acceptance, never transport.
        // This also reserves each FIFO ordinal until its entry reaches the map.
        let admission = self.send_admission.lock().await;
        self.ensure_running()?;
        if let Some(id) = &options.msg_id {
            if let Some(prior) = self.prior_send(&from, id).await? {
                let key = (from.to_string(), id.clone());
                if options.cancel {
                    drop(admission);
                    let _recipient = self.recipient_guard(&prior.entry.target.address()).await;
                    // A delivery may have committed while cancellation waited.
                    let current = self.outbox.lock().await.get(&key).cloned();
                    if let Some(current) = current {
                        let failed = status_receipt(&current.entry, PeerDeliveryState::Failed, Some("cancelled by sender; this does not recall an admission whose reply was lost".into()));
                        self.record_status(&from, &failed).await?;
                        self.outbox.lock().await.remove(&key);
                        return Ok(failed);
                    }
                    return self.completed_receipt(&key).await;
                }
                let same_target =
                    resolve_address(&to, std::slice::from_ref(&prior.entry.target)).is_ok();
                if !same_target
                    || prior.entry.message.message != text
                    || summary != prior.entry.message.summary
                    || options.ttl_ms.is_some_and(|ttl| {
                        prior
                            .entry
                            .message
                            .expires_at
                            .saturating_sub(prior.entry.message.queued_at)
                            != ttl
                    })
                {
                    return Err(PeerError::Invalid {
                        message: format!("msg_id {id:?} already names a different peer send"),
                    });
                }
                return Ok(prior.receipt);
            }
            if options.cancel {
                return Err(PeerError::Invalid {
                    message: format!("unknown peer msg_id {id:?}"),
                });
            }
        }
        validate_text("peer address", &to, PEER_ADDRESS_MAX_BYTES, false)?;
        validate_header("peer address", &to)?;
        validate_text("peer message", &text, PEER_MESSAGE_MAX_BYTES, false)?;
        if let Some(summary) = summary.as_deref() {
            validate_text("peer summary", summary, PEER_SUMMARY_MAX_BYTES, true)?;
        }
        let ttl = options.ttl_ms.unwrap_or(DEFAULT_TTL_MS);
        if ttl == 0 || ttl > MAX_TTL_MS {
            return Err(PeerError::Invalid {
                message: format!("peer ttl_ms must be 1..={MAX_TTL_MS}"),
            });
        }
        self.reconcile_once().await?;
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
        let agents = self.known_peers().await?;
        let target = resolve_address(&to, &agents)?;
        let msg_id = match options.msg_id {
            Some(id) => id,
            None => random_id("msg")?,
        };
        let queued_at = now_ms();
        let enqueue_order = self
            .outbox
            .lock()
            .await
            .values()
            .filter_map(|pending| pending.entry.enqueue_order)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| PeerError::Unavailable {
                message: "peer outbox ordering space exhausted".into(),
            })?;
        let entry = PeerOutboxEntry {
            enqueue_order: Some(enqueue_order),
            message: PeerMessage {
                msg_id: msg_id.clone(),
                from: PeerSender {
                    id: sender.id,
                    device_id: sender.device_id,
                    name: sender.name,
                    kind: PeerKind::HaiderSession,
                    trust: PeerTrust::VerifiedHaider,
                    mode: if sender.state == PeerState::Busy {
                        "prompting"
                    } else {
                        "idle"
                    }
                    .into(),
                },
                to: target.id.clone(),
                message: text.into(),
                summary,
                queued_at,
                expires_at: queued_at.saturating_add(ttl),
            },
            target,
        };
        let full = {
            let outbox = self.outbox.lock().await;
            outbox.len() >= MAX_PENDING
                || outbox
                    .values()
                    .filter(|pending| pending.entry.target.address() == entry.target.address())
                    .count()
                    >= MAX_PER_RECIPIENT
        };
        let receipt = status_receipt(
            &entry,
            if full {
                PeerDeliveryState::Failed
            } else {
                PeerDeliveryState::Accepted
            },
            full.then(|| "sender outbox is full for this recipient or daemon".into()),
        );
        // Both facts commit atomically under the ordinary actor/store policy.
        let journal_seq = journal(
            &self.hub()?,
            &from,
            vec![
                EventPayload::PeerOutbox(entry.clone()),
                EventPayload::PeerDelivery(receipt.clone()),
            ],
        )
        .await?;
        self.hub()?.publish_peer_event(
            &from,
            WireFrame::PeerDeliveryChanged {
                receipt: receipt.clone(),
            },
        );
        if full {
            return Ok(receipt);
        }
        let key = (from.to_string(), msg_id);
        let recipient = entry.target.address();
        self.outbox.lock().await.insert(
            key.clone(),
            Pending {
                entry,
                receipt,
                journal_seq,
            },
        );
        drop(admission);
        let guard = self.recipient_guard(&recipient).await;
        self.attempt(&key, &guard).await
    }

    pub(crate) async fn has_pending_sends(&self) -> bool {
        !self.outbox.lock().await.is_empty() || self.sends.available_permits() < SEND_CAPACITY
    }

    async fn record_status(
        &self,
        from: &SessionId,
        receipt: &PeerReceipt,
    ) -> Result<(), PeerError> {
        let hub = self.hub()?;
        journal(
            &hub,
            from,
            vec![EventPayload::PeerDelivery(receipt.clone())],
        )
        .await?;
        hub.publish_peer_event(
            from,
            WireFrame::PeerDeliveryChanged {
                receipt: receipt.clone(),
            },
        );
        Ok(())
    }

    /// Lock order: acceptance -> outbox, or recipient -> outbox. Acceptance
    /// is released before waiting for a recipient. The guard registry is only
    /// held to upgrade/create a mutex, never while waiting for that mutex.
    /// Outbox guards never span I/O or acquisition of another lock. Every
    /// caller holds a `sends` permit until after its recipient guard drops,
    /// including maintenance, so terminal removal cannot enable retirement
    /// while transport/status commit is still active.
    async fn recipient_guard(&self, address: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut guards = self.recipient_guards.lock().await;
            guards.retain(|_, guard| guard.strong_count() != 0);
            match guards.get(address).and_then(std::sync::Weak::upgrade) {
                Some(guard) => guard,
                None => {
                    let guard = Arc::new(tokio::sync::Mutex::new(()));
                    guards.insert(address.to_owned(), Arc::downgrade(&guard));
                    guard
                }
            }
        };
        mutex.lock_owned().await
    }

    async fn completed_receipt(&self, key: &Key) -> Result<PeerReceipt, PeerError> {
        self.prior_send(&SessionId::new(key.0.clone()), &key.1)
            .await?
            .map(|prior| prior.receipt)
            .ok_or_else(|| PeerError::Invalid {
                message: "peer outbox entry disappeared".into(),
            })
    }

    async fn attempt(
        self: &Arc<Self>,
        key: &Key,
        _recipient: &tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<PeerReceipt, PeerError> {
        let snapshot = {
            let outbox = self.outbox.lock().await;
            outbox.get(key).map(|pending| {
                let older_pending = outbox.iter().any(|(other_key, other)| {
                    other.entry.target.address() == pending.entry.target.address()
                        && order_key(other_key, other) < order_key(key, pending)
                });
                (pending.clone(), older_pending)
            })
        };
        let Some((pending, older_pending)) = snapshot else {
            // Another send/drain/cancel may have settled this key before the
            // recipient guard became available. Never retransmit a terminal key.
            return self.completed_receipt(key).await;
        };
        let entry = &pending.entry;
        let receipt = if now_ms() >= entry.message.expires_at {
            status_receipt(entry, PeerDeliveryState::Failed, Some("expired before confirmed receiver admission; a lost reply can hide an admission".into()))
        } else if older_pending {
            status_receipt(
                entry,
                PeerDeliveryState::Held,
                Some("waiting for older pending send to this recipient".into()),
            )
        } else {
            let result = self.deliver_registered(entry).await;
            match result {
                Ok(received) if received.msg_id != entry.message.msg_id => status_receipt(
                    entry,
                    PeerDeliveryState::Failed,
                    Some("endpoint returned a different msg_id".into()),
                ),
                Ok(received) => {
                    let state = received.status.as_ref().map_or_else(
                        || match received.delivery {
                            PeerDelivery::Delivered | PeerDelivery::Queued => {
                                PeerDeliveryState::Delivered
                            }
                            _ => PeerDeliveryState::Failed,
                        },
                        |status| status.state,
                    );
                    let reason = received
                        .status
                        .as_ref()
                        .and_then(|status| status.reason.clone())
                        .or_else(|| {
                            received
                                .reason
                                .map(|reason| format!("receiver: {reason:?}"))
                        });
                    let mut receipt = status_receipt(entry, state, reason);
                    if state == PeerDeliveryState::Delivered
                        && matches!(
                            received.delivery,
                            PeerDelivery::Queued | PeerDelivery::Delivered
                        )
                    {
                        receipt.delivery = received.delivery;
                    }
                    receipt
                }
                Err(error) => {
                    let state =
                        if matches!(error, PeerError::Refused { .. } | PeerError::Invalid { .. }) {
                            PeerDeliveryState::Failed
                        } else {
                            PeerDeliveryState::Held
                        };
                    status_receipt(
                        entry,
                        state,
                        Some(format!(
                            "{} ({}): {error}",
                            entry.target.address(),
                            entry.target.name
                        )),
                    )
                }
            }
        };
        // Repeated identical offline probes do not flood the journal.
        let same = match (&pending.receipt.status, &receipt.status) {
            (Some(old), Some(new)) => old.state == new.state && old.reason == new.reason,
            _ => false,
        };
        if same {
            return Ok(pending.receipt);
        }
        self.record_status(&SessionId::new(key.0.clone()), &receipt)
            .await?;
        let mut outbox = self.outbox.lock().await;
        if is_pending(&receipt) {
            if let Some(pending) = outbox.get_mut(key) {
                pending.receipt = receipt.clone();
            }
        } else {
            outbox.remove(key);
        }
        Ok(receipt)
    }

    pub(super) async fn drain_outbox(self: &Arc<Self>) -> Result<(), PeerError> {
        let recipients = {
            let outbox = self.outbox.lock().await;
            let mut keys = outbox.keys().cloned().collect::<Vec<_>>();
            keys.sort_by(|left, right| {
                order_key(left, &outbox[left]).cmp(&order_key(right, &outbox[right]))
            });
            let mut recipients = BTreeMap::<String, Vec<Key>>::new();
            for key in keys {
                recipients
                    .entry(outbox[&key].entry.target.address())
                    .or_default()
                    .push(key);
            }
            recipients
        };
        // Poll recipients concurrently, but keep each recipient's FIFO under
        // one guard through transport and status commits. Collect all results:
        // an error must not cancel another recipient mid-journal commit.
        let results = stream::iter(recipients)
            .map(|(address, keys)| async move {
                let Ok(permit) = self.sends.clone().try_acquire_owned() else {
                    return Ok(()); // The next maintenance pass retries it.
                };
                let guard = self.recipient_guard(&address).await;
                let mut result = Ok(());
                let mut settled = std::collections::BTreeSet::new();
                for key in &keys {
                    if self.draining.load(Ordering::Acquire) {
                        break;
                    }
                    match self.attempt(key, &guard).await {
                        Ok(receipt) if !is_pending(&receipt) => {
                            settled.insert(key.0.clone());
                        }
                        Ok(_) => {}
                        Err(error) => {
                            result = Err(error);
                            break;
                        }
                    }
                }
                drop(guard);
                drop(permit);
                if let Ok(hub) = self.hub() {
                    for from in settled {
                        hub.notify_peer_delivery_settled(SessionId::new(from));
                    }
                }
                result
            })
            .buffer_unordered(SEND_CAPACITY)
            .collect::<Vec<Result<(), PeerError>>>()
            .await;
        results.into_iter().collect()
    }

    async fn deliver_registered(
        self: &Arc<Self>,
        entry: &PeerOutboxEntry,
    ) -> Result<PeerReceipt, PeerError> {
        // Revalidate registration; a removed/replaced manifest revokes delivery.
        let agents = self.known_peers().await?;
        let target = agents
            .iter()
            .find(|peer| peer.address() == entry.target.address() && peer.kind == entry.target.kind)
            .ok_or_else(|| PeerError::Refused {
                message: format!(
                    "registration removed for {}; candidates: {}",
                    entry.target.address(),
                    agents
                        .iter()
                        .map(PeerDescriptor::address)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })?;
        if self.is_local(&target.id)? && target.device_id == self.hub()?.peer_device_id() {
            return self.enqueue_local(entry.message.clone()).await;
        }
        #[cfg(unix)]
        {
            let path = endpoint_path_for(&self.runtime_dir, target)?;
            tokio::time::timeout(
                Duration::from_secs(4),
                exchange_delivery(&path, entry.message.clone(), &target.device_id),
            )
            .await
            .map_err(|_| PeerError::Unavailable {
                message: format!("connect/admission deadline elapsed at {}", path.display()),
            })?
        }
        #[cfg(windows)]
        Err(PeerError::Refused {
            message: "cross-daemon peer transport is unavailable on Windows".into(),
        })
    }

    pub(super) async fn known_peers(&self) -> Result<Vec<PeerDescriptor>, PeerError> {
        #[cfg(unix)]
        {
            let directory = self.runtime_dir.clone();
            let manifests = tokio::task::spawn_blocking(move || manifest_candidates(&directory))
                .await
                .map_err(|error| PeerError::Unavailable {
                    message: format!("peer registration scan failed: {error}"),
                })??;
            let mut agents = manifests
                .into_iter()
                .map(|(manifest, _)| PeerDescriptor {
                    id: manifest.id,
                    device_id: manifest.device_id,
                    name: manifest.name,
                    kind: manifest.kind,
                    workspace: manifest.workspace,
                    model: manifest.model,
                    state: manifest.state,
                    started_at: manifest.started_at,
                    last_seen: manifest.last_seen,
                })
                .collect::<Vec<_>>();
            // Local identity remains authoritative over external collisions.
            let publications = self
                .publications
                .lock()
                .map_err(|_| PeerError::Unavailable {
                    message: "peer registry is poisoned".into(),
                })?;
            agents.retain(|agent| !publications.contains_key(&agent.id));
            agents.extend(
                publications
                    .values()
                    .map(|publication| publication.descriptor.clone()),
            );
            Ok(deduplicate_agents(agents))
        }
        #[cfg(windows)]
        self.discover().await
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::peer_tests::live_peer_fixture;
    use haider_core::StoreHandle;

    #[tokio::test]
    async fn stalled_recipient_serializes_its_sends_without_blocking_other_recipients() {
        let root = tempfile::tempdir_in("/tmp").expect("short root");
        let runtime = root.path().join("r");
        let (sh, ss, sender) = live_peer_fixture(&root.path().join("s"), &runtime, "sender").await;
        let (th, ts, target) = live_peer_fixture(&root.path().join("t"), &runtime, "target").await;
        let (hh, hs, healthy) =
            live_peer_fixture(&root.path().join("h"), &runtime, "healthy").await;
        target.shutdown().await;
        let paths =
            peer_endpoint_paths(&runtime, "target", PeerEndpointKind::Haider).expect("paths");
        let listener = tokio::net::UnixListener::bind(&paths.socket).expect("stalled endpoint");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))
            .expect("private socket");
        let from = SessionId::new("sender");
        let send = |to: &str, id: &str| {
            let sender = sender.clone();
            let from = from.clone();
            let to = to.to_owned();
            let id = id.to_owned();
            tokio::spawn(async move {
                sender
                    .send_with_options(
                        &from,
                        to,
                        id.clone(),
                        None,
                        PeerSendOptions {
                            msg_id: Some(id),
                            ..Default::default()
                        },
                    )
                    .await
            })
        };
        let manager = crate::peer_tests::start_held_peer_turn(
            &hh,
            &hs,
            &SessionId::new("healthy"),
            &haider_protocol::ids::RunId::new("healthy-busy"),
        )
        .await;
        let head = send("target", "head");
        let (connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("head reached transport")
            .expect("connection");
        let tail = send("target", "tail");
        let duplicate = send("target", "head");
        let delivered =
            tokio::time::timeout(Duration::from_secs(2), send("healthy", "independent"))
                .await
                .expect("healthy send must finish while target is stalled")
                .expect("send task")
                .expect("healthy receipt");
        assert_eq!(
            delivered.status.expect("status").state,
            PeerDeliveryState::Delivered
        );
        assert!(!head.is_finished());
        assert!(!tail.is_finished());
        let duplicate = duplicate
            .await
            .expect("duplicate task")
            .expect("duplicate receipt");
        assert_eq!(duplicate.msg_id, "head");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "a second same-recipient transport must not enter while the head is held"
        );
        assert!(
            !sh.daemon_is_durably_quiescent()
                .await
                .expect("inflight check")
        );
        drop(connection);
        head.await.expect("head task").expect("held receipt");
        let tail = tail.await.expect("tail task").expect("tail receipt");
        assert_eq!(
            tail.status.expect("tail status").state,
            PeerDeliveryState::Held
        );
        let entries = ss.read(&from, 0, 256).await.expect("journal");
        assert_eq!(
            entries
                .iter()
                .filter(|event| matches!(event.payload.decode_event(),
            Ok(EventPayload::PeerOutbox(entry)) if entry.message.msg_id == "head"))
                .count(),
            1,
            "concurrent duplicate must have only one durable acceptance"
        );
        drop(listener);
        sender.shutdown().await;
        healthy.shutdown().await;
        manager.shutdown().await.expect("worker stop");
        sh.shutdown().await.expect("sender stop");
        th.shutdown().await.expect("target stop");
        hh.shutdown().await.expect("healthy stop");
        ss.close().await.expect("sender close");
        ts.close().await.expect("target close");
        hs.close().await.expect("healthy close");
    }

    #[tokio::test]
    async fn durable_fifo_order_survives_timestamp_ties_restart_and_new_sends() {
        let root = tempfile::tempdir_in("/tmp").expect("short root");
        let runtime = root.path().join("r");
        let sender_root = root.path().join("s");
        let target_root = root.path().join("t");
        let (sh, ss, sender) = live_peer_fixture(&sender_root, &runtime, "sender").await;
        let (th, ts, target) = live_peer_fixture(&target_root, &runtime, "target").await;
        target.shutdown().await;
        let from = SessionId::new("sender");
        sender
            .send_with_options(
                &from,
                "target".into(),
                "first".into(),
                None,
                PeerSendOptions {
                    msg_id: Some("z-first".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("offline send");
        let mut first = sender
            .outbox
            .lock()
            .await
            .values()
            .next()
            .expect("first entry")
            .entry
            .clone();
        sender.shutdown().await;
        // Journal two accepted entries at exactly the same timestamp, with
        // message IDs deliberately in reverse lexical order. This represents
        // the clock tie that a timestamp-sort cannot resolve after restart.
        first.message.queued_at = now_ms();
        first.message.expires_at = first.message.queued_at + 60_000;
        let mut second = first.clone();
        second.message.msg_id = "a-second".into();
        second.message.message = "second".into();
        second.enqueue_order = Some(first.enqueue_order.expect("durable order") + 1);
        for entry in [first, second] {
            let receipt = status_receipt(&entry, PeerDeliveryState::Accepted, None);
            journal(
                &sh,
                &from,
                vec![
                    EventPayload::PeerOutbox(entry),
                    EventPayload::PeerDelivery(receipt),
                ],
            )
            .await
            .expect("timestamp tie journal");
        }
        sh.shutdown().await.expect("sender stop");
        ss.close().await.expect("sender close");
        th.shutdown().await.expect("target stop");
        ts.close().await.expect("target close");
        let (sh, ss, sender) = live_peer_fixture(&sender_root, &runtime, "sender").await;
        assert_eq!(sender.outbox.lock().await.len(), 2);
        let third = sender
            .send_with_options(
                &from,
                "target".into(),
                "third".into(),
                None,
                PeerSendOptions {
                    msg_id: Some("0-third".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("new send after restart");
        assert_eq!(third.status.expect("status").state, PeerDeliveryState::Held);
        let permit = sender
            .sends
            .clone()
            .acquire_owned()
            .await
            .expect("send permit");
        let address = sender
            .outbox
            .lock()
            .await
            .values()
            .next()
            .expect("pending")
            .entry
            .target
            .address();
        let guard = sender.recipient_guard(&address).await;
        let (th, ts, target) = live_peer_fixture(&target_root, &runtime, "target").await;
        // A live held turn supplies the real receiver's durable admission path.
        let manager = crate::peer_tests::start_held_peer_turn(
            &th,
            &ts,
            &SessionId::new("target"),
            &haider_protocol::ids::RunId::new("fifo-busy"),
        )
        .await;
        let tail = (from.to_string(), "a-second".to_owned());
        let held = sender
            .attempt(&tail, &guard)
            .await
            .expect("ready tail attempt");
        assert_eq!(
            held.status.expect("tail status").state,
            PeerDeliveryState::Held,
            "a ready tail must wait until its older pending head terminates"
        );
        drop(guard);
        drop(permit);
        sender.drain_outbox().await.expect("reconnect drain");
        let events = ts
            .read(&SessionId::new("target"), 0, 256)
            .await
            .expect("receiver journal");
        let ids = events
            .iter()
            .filter_map(|event| match event.payload.decode_event() {
                Ok(EventPayload::PeerMessage(message)) => Some(message.msg_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, ["z-first", "a-second", "0-third"]);
        assert!(
            sh.daemon_is_durably_quiescent()
                .await
                .expect("settled quiescence")
        );
        sender.shutdown().await;
        target.shutdown().await;
        manager.shutdown().await.expect("worker stop");
        sh.shutdown().await.expect("sender stop");
        th.shutdown().await.expect("target stop");
        ss.close().await.expect("sender close");
        ts.close().await.expect("target close");
    }
}
