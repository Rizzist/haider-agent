//! CHARTER — the attachment delivery pipeline (§5.5 steps 4-7).
//!
//! What lives here: one task per attachment ([`run_replay`]) and every paced
//! frame it emits — store replay, `AttachCaughtUp`, buffered drain, live
//! delivery, lag/store-resume, the drain-time final suffix, and the
//! detach-with-`Lagged` exit. Two laws are stated authoritatively in this
//! file: the PACING LAW (on [`deliver_frame`]) and the UNKNOWN-ID RULE (on
//! [`lag_and_detach`]). What may NOT live here: store mutation (all appends
//! and CAS go through the session actor — actor.rs), attachment ownership
//! bookkeeping (mod.rs's `attachments`/`attachment_slots` maps own admission
//! and release; this task only borrows its registration), and RPC semantics
//! (rpc.rs). A replay task never writes the store and never answers requests.

use super::*;

// ──────── replay pipeline: replay → caught-up → buffered drain → live ───────

pub(super) enum ReplayCompletion {
    Complete,
    FinalSuffixFailed(FinalSuffixFailure),
}

pub(super) struct FinalSuffixFailure {
    pub(super) stage: &'static str,
    pub(super) message: String,
}

/// One attachment's delivery task, §5.5 steps 4-7.
///
/// Each outer iteration replays `(last_sent_seq, H]` from store pages, then
/// announces `AttachCaughtUp(H)`, drains the already-registered bounded
/// receiver for `seq > H` (duplicates dropped by seq), and goes live. A
/// lagged or overflowed receiver re-registers in actor order and re-enters
/// the outer loop with the new head — the store, not memory, carries what was
/// missed. Exit discipline: `break` still owns the attachment registration
/// and releases it at the bottom; every `return` path has already released
/// ownership (via `lag_and_detach`/`take_attachment`), observed its
/// cancellation, or reported a final-suffix failure for shutdown's owner
/// sweep.
pub(super) async fn run_replay(
    hub: SessionHub,
    mut registration: Registration,
    mut last_sent_seq: u64,
    sealed_replay: bool,
    sink: Arc<dyn FrameSink>,
    mut cancel: watch::Receiver<bool>,
) -> ReplayCompletion {
    let attachment_id = registration.attachment_id.clone();
    let session_id = registration.attach_state.session_id.clone();
    let mut high_water = registration.attach_state.replay_through_seq;
    // Sealing applies only until this attachment announces its initial
    // durable high-water mark. Any buffered/live tail (including a later
    // store resume after receiver lag) must remain a faithful event stream.
    let mut seal_store_replay = sealed_replay;
    loop {
        // Phase: store replay of (last_sent_seq, high_water].
        let replayed = replay_range(
            &hub,
            &sink,
            &attachment_id,
            &session_id,
            &mut last_sent_seq,
            high_water,
            seal_store_replay,
            &mut registration.lagged,
            &mut cancel,
        )
        .await;
        match replayed {
            ReplayStep::Continue => {}
            ReplayStep::ReceiverLagged => {
                match reregister(&hub, &registration.actor, &attachment_id).await {
                    Some((events, lagged, next_head)) => {
                        registration.events = events;
                        registration.lagged = lagged;
                        high_water = next_head;
                        continue;
                    }
                    None => {
                        // Actor gone (graceful drain) with lag pending: the
                        // committed suffix must still broadcast (§6.6).
                        if let Err(failure) = final_suffix_resume(
                            &hub,
                            &sink,
                            &attachment_id,
                            &session_id,
                            &mut last_sent_seq,
                            seal_store_replay,
                            &mut registration.lagged,
                            &mut cancel,
                        )
                        .await
                        {
                            return ReplayCompletion::FinalSuffixFailed(failure);
                        }
                        break;
                    }
                }
            }
            ReplayStep::Cancelled | ReplayStep::ReadFailed(_) => break,
            ReplayStep::OutboxFull => {
                lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                return ReplayCompletion::Complete;
            }
        }

        // Phase: (after_seq, H] is fully on the wire — announce H.
        hub.inner.observer.observe(HubObservation::BeforeCaughtUp {
            attachment_id: attachment_id.clone(),
            through_seq: high_water,
        });
        let caught_up = WireFrame::AttachCaughtUp {
            attachment_id: attachment_id.clone(),
            high_water_seq: high_water,
        };
        match deliver_frame(
            &hub,
            &sink,
            &attachment_id,
            &caught_up,
            &mut registration.lagged,
            &mut cancel,
        )
        .await
        {
            FrameDelivery::Delivered => {}
            FrameDelivery::Cancelled => return ReplayCompletion::Complete,
            FrameDelivery::Stuck | FrameDelivery::Refused => {
                lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                return ReplayCompletion::Complete;
            }
        }
        hub.inner.observer.observe(HubObservation::CaughtUp {
            attachment_id: attachment_id.clone(),
            through_seq: high_water,
        });
        seal_store_replay = false;

        // Phase: buffered drain — deliver `seq > H` already committed during
        // replay, dropping duplicates by seq (at-least-once, R11).
        loop {
            if *cancel.borrow() {
                return ReplayCompletion::Complete;
            }
            match registration.events.try_recv() {
                Ok(queued) => {
                    credit_catch_up(&registration.catch_up_bytes, queued.weight);
                    let envelope = queued.envelope;
                    if envelope.seq <= last_sent_seq || envelope.seq <= high_water {
                        continue;
                    }
                    match deliver_event(
                        &hub,
                        &sink,
                        &attachment_id,
                        &session_id,
                        envelope.as_ref(),
                        &mut last_sent_seq,
                        DeliveryPhase::Buffered,
                        &mut registration.lagged,
                        &mut cancel,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(FrameDelivery::Cancelled) => return ReplayCompletion::Complete,
                        Err(_) => {
                            lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                            return ReplayCompletion::Complete;
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if registration.lagged.borrow().is_some() {
                        match reregister(&hub, &registration.actor, &attachment_id).await {
                            Some((events, lagged, next_head)) => {
                                registration.events = events;
                                registration.lagged = lagged;
                                high_water = next_head;
                                break;
                            }
                            None => {
                                if let Err(failure) = final_suffix_resume(
                                    &hub,
                                    &sink,
                                    &attachment_id,
                                    &session_id,
                                    &mut last_sent_seq,
                                    seal_store_replay,
                                    &mut registration.lagged,
                                    &mut cancel,
                                )
                                .await
                                {
                                    return ReplayCompletion::FinalSuffixFailed(failure);
                                }
                                return ReplayCompletion::Complete;
                            }
                        }
                    }
                    return ReplayCompletion::Complete;
                }
            }
        }
        if high_water > last_sent_seq {
            // A mid-drain re-registration raised the head: replay the gap
            // from the store before going live.
            continue;
        }

        // Phase: live — wait on cancellation, the next commit, or lag. The
        // events arm precedes the lag arm deliberately: when the actor dies
        // (graceful drain) the lag watch closes while buffered envelopes may
        // remain, and this order drains that tail — the §6.6 final broadcast
        // — before `recv` returns `None` on the closed channel. A real lag
        // is only raised AFTER the sender stopped buffering, so draining
        // queued items first never delays its handling.
        loop {
            tokio::select! {
                biased;
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        return ReplayCompletion::Complete;
                    }
                }
                queued = registration.events.recv() => {
                    let Some(queued) = queued else {
                        // Channel closed and fully drained (actor gone). A
                        // pending lag means committed envelopes overflowed
                        // past this channel: stream them from the store
                        // before exiting (§6.6 final broadcast).
                        if registration.lagged.borrow().is_some()
                            && let Err(failure) = final_suffix_resume(
                                &hub,
                                &sink,
                                &attachment_id,
                                &session_id,
                                &mut last_sent_seq,
                                seal_store_replay,
                                &mut registration.lagged,
                                &mut cancel,
                            )
                            .await
                        {
                            return ReplayCompletion::FinalSuffixFailed(failure);
                        }
                        return ReplayCompletion::Complete;
                    };
                    credit_catch_up(&registration.catch_up_bytes, queued.weight);
                    let envelope = queued.envelope;
                    if envelope.seq <= last_sent_seq {
                        continue;
                    }
                    match deliver_event(
                        &hub,
                        &sink,
                        &attachment_id,
                        &session_id,
                        envelope.as_ref(),
                        &mut last_sent_seq,
                        DeliveryPhase::Live,
                        &mut registration.lagged,
                        &mut cancel,
                    ).await {
                        Ok(()) => {}
                        Err(FrameDelivery::Cancelled) => return ReplayCompletion::Complete,
                        Err(_) => {
                            lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                            return ReplayCompletion::Complete;
                        }
                    }
                }
                changed = registration.lagged.changed() => {
                    if changed.is_ok() && registration.lagged.borrow().is_some() {
                        match reregister(&hub, &registration.actor, &attachment_id).await {
                            Some((events, lagged, next_head)) => {
                                registration.events = events;
                                registration.lagged = lagged;
                                high_water = next_head;
                                break;
                            }
                            None => {
                                if let Err(failure) = final_suffix_resume(
                                    &hub,
                                    &sink,
                                    &attachment_id,
                                    &session_id,
                                    &mut last_sent_seq,
                                    seal_store_replay,
                                    &mut registration.lagged,
                                    &mut cancel,
                                )
                                .await
                                {
                                    return ReplayCompletion::FinalSuffixFailed(failure);
                                }
                                return ReplayCompletion::Complete;
                            }
                        }
                    }
                    // A closed watch (actor gone) loops back into the events
                    // arm, which always resolves on a closed channel — the
                    // pending-lag check there owns the terminal resume.
                }
            }
        }
    }
    // `break` exit: store failure, truncated page, or a closed actor. The
    // registration is still owned here; release it (a no-op if a concurrent
    // detach already took it).
    let _ = hub.detach(&attachment_id).await;
    ReplayCompletion::Complete
}

enum ReplayStep {
    Continue,
    ReceiverLagged,
    Cancelled,
    ReadFailed(String),
    OutboxFull,
}

/// Terminal outcome of one paced frame delivery.
pub(super) enum FrameDelivery {
    Delivered,
    Cancelled,
    /// Lag pressure arrived while the sink was busy: commits are overflowing
    /// the catch-up buffer behind a stalled outbox — the genuinely stuck
    /// client shape. The caller laggs and detaches.
    Stuck,
    /// The sink refused outright (closed, over the negotiated limit, or a
    /// pairing-contract violation). The caller laggs and detaches.
    Refused,
}

/// Delivers one frame under the pacing law.
///
/// PACING LAW (authoritative statement): every attachment frame is admitted
/// through the sink's atomic both-dimension [`FrameSink::offer`] — the
/// admission is the reservation, granted and consumed in one step under the
/// sink's lock, so concurrent lanes cannot race a capacity snapshot and
/// overbook, and a byte-bound sink cannot falsely refuse a reading client.
/// A `Busy` answer makes this task take one FIFO
/// [`FrameSink::drain_ticket`] before its confirming re-offer. The token
/// remains the head reservation across every wake/re-offer until admission,
/// so notification cannot be barged and service itself is FIFO. Detachment
/// happens only on [`SendAdmission::Refused`] or when lag pressure arrives
/// while the sink is busy ([`FrameDelivery::Stuck`]).
pub(super) async fn deliver_frame(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    frame: &WireFrame,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> FrameDelivery {
    let Ok(frame) = sink.prepare(frame) else {
        return FrameDelivery::Refused;
    };
    deliver_prepared_frame(hub, sink, attachment_id, &frame, lagged, cancel).await
}

async fn deliver_prepared_frame(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    frame: &PreparedFrame,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> FrameDelivery {
    if *cancel.borrow() {
        return FrameDelivery::Cancelled;
    }
    match hub.offer_attachment_prepared(attachment_id, sink, frame) {
        SendAdmission::Sent => return FrameDelivery::Delivered,
        SendAdmission::Refused => return FrameDelivery::Refused,
        SendAdmission::Busy => {}
    }
    let Some(ticket) = sink.drain_ticket() else {
        // Pairing-contract violation (see [`FrameSink`]): Busy without a
        // ticket source degrades to refusal instead of spinning.
        return FrameDelivery::Refused;
    };
    let mut ticket = AdmissionTicketGuard::new(Arc::clone(sink), ticket);
    loop {
        // Confirming and later re-offers retain the SAME head token: capacity
        // freed between the Busy answer and this call cannot be lost, and no
        // fresh offer may consume it first.
        match hub.offer_attachment_prepared_ticketed(attachment_id, sink, frame, ticket.ticket()) {
            SendAdmission::Sent => {
                ticket.disarm();
                return FrameDelivery::Delivered;
            }
            SendAdmission::Refused => {
                ticket.cancel();
                return FrameDelivery::Refused;
            }
            SendAdmission::Busy => {}
        }
        // A closed lag watch means the actor is gone (graceful drain): no
        // further commits can pile up, so the stuck signature is impossible
        // and the wait continues on the ticket alone.
        let lag_open = lagged.has_changed().is_ok();
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    ticket.cancel();
                    return FrameDelivery::Cancelled;
                }
            }
            changed = lagged.changed(), if lag_open => {
                if changed.is_ok() && lagged.borrow().is_some() {
                    ticket.cancel();
                    return FrameDelivery::Stuck;
                }
            }
            _ = ticket.ticket().notified() => {
                #[cfg(test)]
                if let Some(gate) = sink.ticket_fired_test_gate() {
                    gate.await;
                }
            }
        }
    }
}

/// Credits bytes back to the shared catch-up ledger as envelopes leave the
/// channel. Saturating: an (impossible by charge/credit symmetry) underflow
/// must degrade to zero, never wrap into a permanent phantom backlog.
fn credit_catch_up(catch_up_bytes: &Arc<AtomicUsize>, weight: usize) {
    let _ = catch_up_bytes.fetch_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
        Some(bytes.saturating_sub(weight))
    });
}

#[allow(clippy::too_many_arguments)]
async fn replay_range(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    last_sent_seq: &mut u64,
    high_water: u64,
    sealed_replay: bool,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> ReplayStep {
    while *last_sent_seq < high_water {
        // Byte-budgeted page (NOW-2): bounds the transient envelopes one page
        // may materialize; a short page just resumes from `last_sent_seq`.
        let read = hub.inner.store.read_page(
            session_id,
            *last_sent_seq,
            REPLAY_PAGE_SIZE,
            hub.inner.config.replay_page_byte_budget,
        );
        // A closed lag watch (actor gone, graceful drain) is not a lag: the
        // store replay keeps streaming its range (§6.6 final broadcast) and
        // the later phases exit on the closed channel.
        let lag_open = lagged.has_changed().is_ok();
        let page = tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    hub.inner.metrics.discarded_store_pages.fetch_add(1, Ordering::Relaxed);
                    return ReplayStep::Cancelled;
                }
                continue;
            }
            changed = lagged.changed(), if lag_open => {
                if changed.is_ok() && lagged.borrow().is_some() {
                    hub.inner.metrics.discarded_store_pages.fetch_add(1, Ordering::Relaxed);
                    return ReplayStep::ReceiverLagged;
                }
                continue;
            }
            result = read => match result {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        attachment_id = %attachment_id,
                        error = ?error,
                        "attachment replay store read failed"
                    );
                    return ReplayStep::ReadFailed(error.message);
                }
            }
        };
        if page.is_empty() {
            return ReplayStep::ReadFailed(format!(
                "store returned an empty replay page before durable head {high_water}"
            ));
        }
        for envelope in page
            .into_iter()
            .take_while(|envelope| envelope.seq <= high_water)
        {
            if *cancel.borrow() {
                return ReplayStep::Cancelled;
            }
            if envelope.seq <= *last_sent_seq {
                continue;
            }
            if sealed_replay && is_item_delta(&envelope) {
                // A skipped durable envelope still advances the replay
                // cursor. CaughtUp therefore reports exactly the same high
                // water as an unsealed replay.
                *last_sent_seq = envelope.seq;
                continue;
            }
            match deliver_event(
                hub,
                sink,
                attachment_id,
                session_id,
                &envelope,
                last_sent_seq,
                DeliveryPhase::Replay,
                lagged,
                cancel,
            )
            .await
            {
                Ok(()) => {}
                Err(FrameDelivery::Cancelled) => return ReplayStep::Cancelled,
                Err(_) => return ReplayStep::OutboxFull,
            }
        }
    }
    ReplayStep::Continue
}

fn is_item_delta(envelope: &RawEnvelope) -> bool {
    let payload = &envelope.payload;
    payload.get("type").and_then(serde_json::Value::as_str) == Some("item")
        && payload.get("event").and_then(serde_json::Value::as_str) == Some("delta")
        // Ship-gate round: a skippable delta must also be ADDRESSABLE — a
        // shape carrying the delta discriminant but no item_id is a corrupt
        // envelope the client should SEE, never silently lose. Subtype
        // fields stay unchecked on purpose: future delta kinds this daemon
        // cannot parse must still seal.
        && payload
            .get("item_id")
            .and_then(serde_json::Value::as_str)
            .is_some()
        // ItemDelta is an internally tagged union. Requiring its string
        // discriminant matches the frozen wire shape without decoding or
        // cloning payloads, while still sealing future delta subtypes that
        // this daemon does not know how to deserialize.
        && payload
            .get("delta")
            .and_then(|delta| delta.get("delta"))
            .and_then(serde_json::Value::as_str)
            .is_some()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn envelope_with_payload(payload: serde_json::Value) -> RawEnvelope {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "event_id": "ev-structural-delta-test",
            "seq": 1,
            "session_id": "s-structural-delta-test",
            "device_id": "d-structural-delta-test",
            "authority_epoch": 0,
            "worker_generation": 0,
            "committed_at_ms": 0,
            "render": {
                "ui": true,
                "durable": true,
                "prompt": "verbatim"
            },
            "payload": payload
        }))
        .expect("raw envelope")
    }

    #[test]
    fn sealed_replay_structurally_skips_known_item_delta() {
        let envelope = envelope_with_payload(serde_json::json!({
            "type": "item",
            "event": "delta",
            "item_id": "it-known",
            "delta": {
                "delta": "command_output",
                "stream": "stdout",
                "chunk_b64": "aGk="
            }
        }));

        assert!(is_item_delta(&envelope));
    }

    #[test]
    fn sealed_replay_structurally_skips_unknown_item_delta_subtype() {
        let envelope = envelope_with_payload(serde_json::json!({
            "type": "item",
            "event": "delta",
            "item_id": "it-future",
            "delta": {
                "delta": "future_stream_kind",
                "future_field": { "opaque": true }
            }
        }));

        assert!(is_item_delta(&envelope));
    }

    #[test]
    fn sealed_replay_delivers_non_item_payload() {
        let envelope = envelope_with_payload(serde_json::json!({
            "type": "run_state",
            "state": "thinking"
        }));

        assert!(!is_item_delta(&envelope));
    }

    #[test]
    fn sealed_replay_delivers_malformed_item_delta_payload() {
        let missing_delta_shape = envelope_with_payload(serde_json::json!({
            "type": "item",
            "event": "delta",
            "item_id": "it-malformed",
            "delta": "not-an-item-delta-object"
        }));
        let malformed_payload = envelope_with_payload(serde_json::json!("not-an-object"));

        assert!(!is_item_delta(&missing_delta_shape));
        assert!(!is_item_delta(&malformed_payload));
    }
}

/// Delivers one envelope through [`deliver_frame`]'s pacing law, advancing
/// the cursor only after the sink admitted it.
#[allow(clippy::too_many_arguments)]
async fn deliver_event(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    envelope: &RawEnvelope,
    last_sent_seq: &mut u64,
    phase: DeliveryPhase,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), FrameDelivery> {
    let seq = envelope.seq;
    hub.inner.observer.observe(HubObservation::BeforeEvent {
        attachment_id: attachment_id.clone(),
        seq,
    });
    let Ok(frame) = sink.prepare_event(attachment_id, session_id, envelope) else {
        return Err(FrameDelivery::Refused);
    };
    match deliver_prepared_frame(hub, sink, attachment_id, &frame, lagged, cancel).await {
        FrameDelivery::Delivered => {}
        stopped => return Err(stopped),
    }
    *last_sent_seq = seq;
    hub.inner.observer.observe(match phase {
        DeliveryPhase::Buffered => HubObservation::BufferedEvent {
            attachment_id: attachment_id.clone(),
            seq,
        },
        DeliveryPhase::Replay => HubObservation::ReplayEvent {
            attachment_id: attachment_id.clone(),
            seq,
        },
        DeliveryPhase::Live => HubObservation::LiveEvent {
            attachment_id: attachment_id.clone(),
            seq,
        },
    });
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum DeliveryPhase {
    Replay,
    Buffered,
    Live,
}

/// Replaces the catch-up channels in actor order after lag/overflow; the
/// actor's `Reregister` arm states the byte-ledger reset rationale.
async fn reregister(
    hub: &SessionHub,
    actor: &SessionActorHandle,
    attachment_id: &AttachmentId,
) -> Option<(
    mpsc::Receiver<QueuedEnvelope>,
    watch::Receiver<Option<u64>>,
    u64,
)> {
    let (events, event_receiver) = mpsc::channel(hub.inner.config.catch_up_capacity);
    let (lagged, lag_receiver) = watch::channel(None);
    let (completed, result) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Reregister {
            attachment_id: attachment_id.clone(),
            events,
            lagged,
            completed,
        })
        .await
        .ok()?;
    let head = result.await.ok().flatten()?;
    hub.inner
        .metrics
        .reregistrations
        .fetch_add(1, Ordering::Relaxed);
    hub.inner
        .metrics
        .store_resumes
        .fetch_add(1, Ordering::Relaxed);
    Some((event_receiver, lag_receiver, head))
}

/// Terminal store-resume during graceful drain (§6.6): the actor died with
/// this attachment's lag pending — or its re-registration is no longer
/// possible — so the committed suffix past `last_sent_seq` is streamed from
/// the durable store and closed with a final `AttachCaughtUp` at the durable
/// head. Runs inside the shutdown grace: the barrier deadline bounds it, and
/// a deadline overrun forces the outcome — a committed envelope is delivered
/// here or the drain reports `Forced`, never silently lost.
#[allow(clippy::too_many_arguments)]
async fn final_suffix_resume(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    last_sent_seq: &mut u64,
    sealed_replay: bool,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), FinalSuffixFailure> {
    let head = hub
        .inner
        .store
        .latest_seq(session_id)
        .await
        .map_err(|error| FinalSuffixFailure {
            stage: "latest_seq",
            message: error.message,
        })?;
    if head <= *last_sent_seq {
        return Ok(());
    }
    hub.inner
        .observer
        .observe(HubObservation::FinalSuffixHeadCaptured {
            attachment_id: attachment_id.clone(),
            head,
        });
    let replayed = replay_range(
        hub,
        sink,
        attachment_id,
        session_id,
        last_sent_seq,
        head,
        sealed_replay,
        lagged,
        cancel,
    )
    .await;
    match replayed {
        ReplayStep::Continue => {}
        ReplayStep::ReadFailed(message) => {
            return Err(FinalSuffixFailure {
                stage: "read_page",
                message,
            });
        }
        ReplayStep::ReceiverLagged => {
            return Err(FinalSuffixFailure {
                stage: "read_page",
                message: "the final suffix receiver reported lag".into(),
            });
        }
        ReplayStep::Cancelled => {
            return Err(FinalSuffixFailure {
                stage: "read_page",
                message: "the final suffix replay was cancelled".into(),
            });
        }
        ReplayStep::OutboxFull => {
            return Err(FinalSuffixFailure {
                stage: "event_enqueue",
                message: "the final suffix event could not enter the attachment outbox".into(),
            });
        }
    }
    let caught_up = WireFrame::AttachCaughtUp {
        attachment_id: attachment_id.clone(),
        high_water_seq: head,
    };
    match deliver_frame(hub, sink, attachment_id, &caught_up, lagged, cancel).await {
        FrameDelivery::Delivered => Ok(()),
        FrameDelivery::Cancelled => Err(FinalSuffixFailure {
            stage: "final_caught_up_enqueue",
            message: "the final AttachCaughtUp enqueue was cancelled".into(),
        }),
        FrameDelivery::Stuck => Err(FinalSuffixFailure {
            stage: "final_caught_up_enqueue",
            message: "the final AttachCaughtUp enqueue stalled under lag pressure".into(),
        }),
        FrameDelivery::Refused => Err(FinalSuffixFailure {
            stage: "final_caught_up_enqueue",
            message: "the final AttachCaughtUp frame was refused".into(),
        }),
    }
}

/// UNKNOWN-ID RULE (authoritative statement): a client never receives a
/// frame referencing an attachment id it has not been told about. `Lagged`
/// is a CONTROL notice riding the system reply lane — after the purge,
/// nothing attachment-keyed is ever enqueued again, so a detached lane
/// cannot be recreated. If the purge reports that the staged attach RESPONSE
/// itself never reached the wire, the client has never heard this id at all:
/// the original request is answered with a correlated, retryable error
/// instead.
async fn lag_and_detach(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    last_queued_seq: u64,
) {
    let Ok(Some(owner)) = hub.take_attachment(attachment_id, None) else {
        return;
    };
    hub.inner
        .metrics
        .outbox_detaches
        .fetch_add(1, Ordering::Relaxed);
    match sink.purge_attachment(attachment_id) {
        Some(request_id) => {
            let _ = sink.try_send(WireFrame::Response {
                request_id,
                body: ResponseBody::Error {
                    code: ERROR_CODE_OVERLOADED.into(),
                    message: "attachment overwhelmed before its response was delivered; \
                              re-attach from your applied cursor"
                        .into(),
                    retryable: true,
                    data: None,
                },
            });
        }
        None => {
            let _ = sink.try_send(WireFrame::Lagged {
                attachment_id: attachment_id.clone(),
                last_queued_seq,
            });
        }
    }
    SessionHub::finish_detach(attachment_id, owner).await;
}
