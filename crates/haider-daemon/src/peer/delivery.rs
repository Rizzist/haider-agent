//! Sender outbox projection. The session journal is the sole authority;
//! this bounded map is rebuilt on boot and never imports retired .q files.

use super::*;
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
}
pub(super) type Outbox = BTreeMap<Key, Pending>;

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
                            Pending { entry, receipt },
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
) -> Result<(), PeerError> {
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
    Ok(())
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
                        found = Some(Pending { entry, receipt });
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
            let _permit = permit;
            service
                .send_owned(from, to, message, summary, options)
                .await
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
        let mut outbox = self.outbox.lock().await;
        self.ensure_running()?;
        if let Some(id) = &options.msg_id {
            if let Some(prior) = self.prior_send(&from, id).await? {
                let key = (from.to_string(), id.clone());
                if options.cancel {
                    if is_pending(&prior.receipt) {
                        let failed = status_receipt(&prior.entry, PeerDeliveryState::Failed, Some("cancelled by sender; this does not recall an admission whose reply was lost".into()));
                        self.record_status(&from, &failed).await?;
                        outbox.remove(&key);
                        return Ok(failed);
                    }
                    return Ok(prior.receipt);
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
        let entry = PeerOutboxEntry {
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
        let full = outbox.len() >= MAX_PENDING
            || outbox
                .values()
                .filter(|pending| pending.entry.target.address() == entry.target.address())
                .count()
                >= MAX_PER_RECIPIENT;
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
        journal(
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
        outbox.insert(key.clone(), Pending { entry, receipt });
        self.attempt(&key, &mut outbox).await
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

    async fn attempt(
        self: &Arc<Self>,
        key: &Key,
        outbox: &mut Outbox,
    ) -> Result<PeerReceipt, PeerError> {
        let pending = outbox.get(key).cloned().ok_or_else(|| PeerError::Invalid {
            message: "peer outbox entry disappeared".into(),
        })?;
        let entry = &pending.entry;
        let receipt = if now_ms() >= entry.message.expires_at {
            status_receipt(entry, PeerDeliveryState::Failed, Some("expired before confirmed receiver admission; a lost reply can hide an admission".into()))
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
        let mut outbox = self.outbox.lock().await;
        let mut keys = outbox.keys().cloned().collect::<Vec<_>>();
        keys.sort_by_key(|key| outbox[key].entry.message.queued_at);
        for key in keys {
            if self.draining.load(Ordering::Acquire) {
                break;
            }
            self.attempt(&key, &mut outbox).await?;
        }
        Ok(())
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
