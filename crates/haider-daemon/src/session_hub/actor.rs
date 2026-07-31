//! CHARTER — the session actor: one session's serialized command order.
//!
//! What lives here: [`run_session_actor`] (every command arm) and the
//! synchronous [`publish`] fan-out it calls. Nothing else. What may NOT live
//! here: provider or tool work (never awaited in an arm — that is R1's
//! hub-actor purity rule; provider latency must never hold attach, menu, or
//! cancel liveness hostage), socket writes, RPC/transport concerns (rpc.rs),
//! and paced delivery (replay.rs). Every await inside a command arm is a
//! store call (the loop's own `recv` is the one other await in the file).
//!
//! Law owned here — **same-generation worker-lease fencing (R1/R5)**: the
//! actor holds at most one [`RegisteredWorker`]; `AcquireWorkerLease`
//! replaces it (revoking the predecessor) in one serialized arm, and the
//! `WorkerAppend`/`WorkerSettleIdle`/`RegisterHarness`/
//! `RegisterRecoveredHarness`/`UnregisterHarness` arms compare the presented
//! [`WorkerLeaseId`] against the current one before touching the store or the
//! registration. The store's `worker_generation` fences RESTARTS; this token
//! fences a superseded supervisor inside one generation, which a
//! generation-only check cannot distinguish.

use super::*;

// ──────────── session actor: the serialized command loop (§5.5) ─────────────

/// One session's entire command order, in one loop, in one task.
///
/// Both §5.5 invariants (module doc) hold by code shape here: the only awaits
/// inside any arm are the store calls (`append`, `create_session`,
/// `accept_turn`, `cancel_turn`, `settle_session_idle`, `resolve_menu`),
/// publication is a synchronous call after they return in the same arm, and
/// the `Register` arm contains no await at all. Adding an await between a
/// store return and its `publish`, or anywhere in `Register`, breaks a law —
/// the forced-boundary tests in `tests/session_hub_tests.rs` will catch it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_session_actor(
    session_id: SessionId,
    mut head: u64,
    mut authority_epoch: u64,
    worker_generation: u64,
    catch_up_byte_budget: usize,
    store: SqliteStoreHandle,
    observer: Arc<dyn SessionHubObserver>,
    metrics: Arc<HubMetrics>,
    force_stop: Arc<AtomicBool>,
    mut commands: mpsc::Receiver<ActorCommand>,
) {
    let mut attachments = HashMap::<AttachmentId, ActorAttachment>::new();
    let mut worker = Option::<RegisteredWorker>::None;
    // Retained after resolution on purpose: a prior-generation retry must
    // reach the durable CAS and receive AlreadyResolved, not be misreported
    // as stale_generation. Lease replacement clears the coordinate.
    let mut recovered_menu = Option::<RecoveredMenuCoordinate>::None;
    // Graceful drain deliberately has NO early-stop here: every command that
    // reached the queue before `Stop` completes its arm, which is what lets
    // an in-flight append/CAS publish during the §6.6 grace. The one fence
    // is the FORCED path ([`HubInner::force_stop`]): a cancelled shutdown
    // must stop an actor resuming from a synchronous boundary from starting
    // one more store command that nothing will observe.
    while let Some(command) = commands.recv().await {
        if force_stop.load(Ordering::Acquire) {
            break;
        }
        match command {
            ActorCommand::Append {
                mut envelopes,
                completed,
            } => {
                // INVARIANT 1 (module doc): the append is awaited here, and
                // `publish` below is synchronous in this same turn.
                let result = store.append(&mut envelopes).await;
                match result {
                    Ok(range) => {
                        head = range.last_seq;
                        if let Some(last) = envelopes.last() {
                            authority_epoch = last.authority_epoch;
                        }
                        observer.observe(HubObservation::Persisted {
                            session_id: session_id.clone(),
                            through_seq: head,
                        });
                        publish(&mut attachments, &envelopes, catch_up_byte_budget, &metrics);
                        observer.observe(HubObservation::Published {
                            session_id: session_id.clone(),
                            through_seq: head,
                        });
                        let _ = completed.send(Ok(envelopes));
                    }
                    Err(error) => {
                        let _ = completed.send(Err(error));
                    }
                }
            }
            ActorCommand::CreateSession { command, completed } => {
                // Same INV-1 shape as ordinary append: the complete metadata +
                // Created + receipt transaction returns before publication,
                // and no await separates that return from `publish`.
                let result = store.create_session(command).await;
                if let Ok(SessionCreateOutcome::Committed { envelope, .. }) = &result {
                    head = envelope.seq;
                    authority_epoch = envelope.authority_epoch;
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    publish(
                        &mut attachments,
                        std::slice::from_ref(envelope.as_ref()),
                        catch_up_byte_budget,
                        &metrics,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::AcceptTurn { command, completed } => {
                // MUTATION CHECK: publishing before this durable transaction
                // returns makes live clients observe an acceptance a restart
                // cannot recover; the live lost-response test must fail.
                let result = store.accept_turn(command).await;
                if let Ok(TurnAcceptOutcome::Committed { envelopes, .. }) = &result {
                    if let Some(last) = envelopes.last() {
                        head = last.seq;
                        authority_epoch = last.authority_epoch;
                    }
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    publish(&mut attachments, envelopes, catch_up_byte_budget, &metrics);
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::AcceptShellExec { command, completed } => {
                // The acceptance receipt and started command item commit in
                // one transaction before publication or worker handoff.
                let result = store.accept_shell_exec(command).await;
                if let Ok(ShellExecAcceptOutcome::Committed { envelopes, .. }) = &result {
                    if let Some(last) = envelopes.last() {
                        head = last.seq;
                        authority_epoch = last.authority_epoch;
                    }
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    publish(&mut attachments, envelopes, catch_up_byte_budget, &metrics);
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::CancelTurn { command, completed } => {
                // PERSIST-BEFORE-WAKE (R5, authoritative statement): the
                // durable `Cancelling` intent commits and publishes BEFORE the
                // registered worker's cancellation wake fires below, in this
                // same arm — a woken supervisor always finds the intent in the
                // journal. The wake is notification only, never the record:
                // worker admission and startup recovery rescan durable run
                // states, so a missed wake delays reconciliation, not truth.
                let result = store.cancel_turn(command).await;
                if let Ok(TurnCancelOutcome::Committed {
                    envelope: Some(envelope),
                    ..
                }) = &result
                {
                    head = envelope.seq;
                    authority_epoch = envelope.authority_epoch;
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    publish(
                        &mut attachments,
                        std::slice::from_ref(envelope.as_ref()),
                        catch_up_byte_budget,
                        &metrics,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    if let Some(wake) = worker
                        .as_ref()
                        .and_then(|worker| worker.cancellation_wake.as_ref())
                    {
                        wake.send_modify(|generation| {
                            *generation = generation.saturating_add(1);
                        });
                    }
                }
                let _ = completed.send(result);
            }
            ActorCommand::WorkerAppend {
                lease_id,
                expected_head,
                mut envelopes,
                completed,
            } => {
                let current = worker.as_ref().map(|worker| &worker.lease_id);
                if current != Some(&lease_id) {
                    let _ = completed.send(Err(HaiderError::new(
                        ErrorCode::SingleWriterViolation,
                        "worker lease was superseded",
                        false,
                    )));
                    continue;
                }
                if let Some(expected_head) = expected_head
                    && head != expected_head
                {
                    let _ = completed.send(Err(HaiderError::new(
                        ErrorCode::Busy,
                        format!(
                            "session history advanced from {expected_head} to {head} during compaction"
                        ),
                        true,
                    )));
                    continue;
                }
                // DURABLE TERMINAL TRUTH: lease identity is necessary but not
                // sufficient. This worker-only store transaction validates the
                // batch against the durable run head atomically with append.
                // Re-routing this to ordinary `append` must make the
                // cancel-before-Done mutation pin fail.
                let result = store.append_worker(envelopes).await;
                match result {
                    Ok(committed) => {
                        envelopes = committed;
                        head = envelopes.last().map_or(head, |envelope| envelope.seq);
                        if let Some(last) = envelopes.last() {
                            authority_epoch = last.authority_epoch;
                        }
                        observer.observe(HubObservation::Persisted {
                            session_id: session_id.clone(),
                            through_seq: head,
                        });
                        publish(&mut attachments, &envelopes, catch_up_byte_budget, &metrics);
                        observer.observe(HubObservation::Published {
                            session_id: session_id.clone(),
                            through_seq: head,
                        });
                        let _ = completed.send(Ok(envelopes));
                    }
                    Err(error) => {
                        let _ = completed.send(Err(error));
                    }
                }
            }
            ActorCommand::WorkerSettleIdle {
                lease_id,
                envelope,
                completed,
            } => {
                let current = worker.as_ref().map(|worker| &worker.lease_id);
                if current != Some(&lease_id) {
                    let _ = completed.send(Err(HaiderError::new(
                        ErrorCode::SingleWriterViolation,
                        "worker lease was superseded",
                        false,
                    )));
                    continue;
                }
                let result = store.settle_session_idle(envelope).await;
                if let Ok(Some(envelope)) = &result {
                    head = envelope.seq;
                    authority_epoch = envelope.authority_epoch;
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    publish(
                        &mut attachments,
                        std::slice::from_ref(envelope),
                        catch_up_byte_budget,
                        &metrics,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::Register {
                attachment_id,
                after_seq,
                events,
                lagged,
                queued_bytes,
                completed,
            } => {
                if after_seq > head {
                    let _ = completed.send(ActorRegisterResult::CursorAhead {
                        requested: after_seq,
                        head,
                    });
                    continue;
                }
                attachments.insert(
                    attachment_id.clone(),
                    ActorAttachment {
                        events,
                        lagged,
                        queued_bytes,
                        last_buffered_seq: head,
                        active: true,
                    },
                );
                observer.observe(HubObservation::ReceiverRegistered {
                    attachment_id: attachment_id.clone(),
                });
                // INVARIANT 2 (module doc): receiver insertion above and this
                // head read are adjacent synchronous statements in one actor
                // turn — no await or yield between them.
                let high_water = head;
                observer.observe(HubObservation::HeadCaptured {
                    attachment_id: attachment_id.clone(),
                    head: high_water,
                });
                let _ = completed.send(ActorRegisterResult::Registered(AttachState {
                    session_id: session_id.clone(),
                    requested_after_seq: after_seq,
                    replay_through_seq: high_water,
                    worker_generation,
                    authority_epoch,
                }));
            }
            ActorCommand::Reregister {
                attachment_id,
                events,
                lagged,
                completed,
            } => {
                let registered = attachments.get_mut(&attachment_id).map(|attachment| {
                    let discarded = attachment
                        .events
                        .max_capacity()
                        .saturating_sub(attachment.events.capacity());
                    metrics
                        .discarded_envelopes
                        .fetch_add(discarded as u64, Ordering::Relaxed);
                    attachment.events = events;
                    attachment.lagged = lagged;
                    // The old channel and everything queued on it are dropped
                    // wholesale, and the replay task credits nothing after it
                    // requests re-registration, so zero is the exact balance.
                    attachment.queued_bytes.store(0, Ordering::Release);
                    attachment.last_buffered_seq = head;
                    attachment.active = true;
                    head
                });
                let _ = completed.send(registered);
            }
            ActorCommand::Detach { attachment_id } => {
                attachments.remove(&attachment_id);
            }
            ActorCommand::MenuAnswer {
                mut command,
                completed,
            } => {
                // The CAS itself (first-committed-wins) is
                // `Store::resolve_menu`'s law; this arm only serializes it
                // with appends and publishes a committed envelope afterwards
                // (INVARIANT 1 shape).
                command.allow_prior_generation = recovered_menu.as_ref().is_some_and(|recovered| {
                    recovered.menu_id == command.answer.menu
                        && recovered.request_seq == command.request_seq
                        && recovered.opening_generation == command.worker_generation
                });
                let outcome = store.resolve_menu(command).await;
                if let Ok(MenuResolutionOutcome::Committed { ref envelope }) = outcome {
                    head = envelope.seq;
                    authority_epoch = envelope.authority_epoch;
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    publish(
                        &mut attachments,
                        std::slice::from_ref(envelope.as_ref()),
                        catch_up_byte_budget,
                        &metrics,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    if let Some(harness) =
                        worker.as_ref().and_then(|worker| worker.harness.as_ref())
                        && let Err(error) =
                            harness.apply_committed_menu_event(envelope.as_ref().clone())
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            error = ?error,
                            "committed menu event could not wake the live harness"
                        );
                    }
                }
                let _ = completed.send(outcome);
            }
            ActorCommand::AcquireWorkerLease {
                lease_id,
                cancellation_wake,
                completed,
            } => {
                // Replacement revokes the old token in this serialized step
                // before the successor can append or register a wake target.
                worker = Some(RegisteredWorker {
                    lease_id,
                    harness: None,
                    cancellation_wake,
                });
                recovered_menu = None;
                let _ = completed.send(());
            }
            ActorCommand::RegisterHarness {
                lease_id,
                harness,
                completed,
            } => {
                let result = match worker.as_mut() {
                    Some(current) if current.lease_id == lease_id => {
                        current.harness = Some(harness);
                        Ok(())
                    }
                    _ => Err(HaiderError::new(
                        ErrorCode::SingleWriterViolation,
                        "worker lease was superseded before harness registration",
                        false,
                    )),
                };
                let _ = completed.send(result);
            }
            ActorCommand::RegisterRecoveredHarness {
                lease_id,
                harness,
                menu,
                completed,
            } => {
                let result = match worker.as_mut() {
                    Some(current) if current.lease_id == lease_id => {
                        current.harness = Some(harness);
                        recovered_menu = Some(menu);
                        Ok(())
                    }
                    _ => Err(HaiderError::new(
                        ErrorCode::SingleWriterViolation,
                        "worker lease was superseded before recovered harness registration",
                        false,
                    )),
                };
                let _ = completed.send(result);
            }
            ActorCommand::UnregisterHarness { lease_id } => {
                if worker
                    .as_ref()
                    .is_some_and(|current| current.lease_id == lease_id)
                {
                    worker = None;
                }
            }
            ActorCommand::Stop => break,
        }
    }
}

/// Fans committed envelopes out to every active attachment receiver.
///
/// Called only from [`run_session_actor`], synchronously, after the store
/// call returned (INVARIANT 1, module doc). `try_send` never blocks the
/// actor; a full receiver — full in FRAMES or in estimated BYTES — flips the
/// attachment inactive and reports its last buffered sequence on the lag
/// channel, and the replay task then resumes from the store
/// (store-is-the-lag-buffer, module doc). Bytes are charged before enqueue
/// and credited by the replay task on receive. The byte bound is HARD: an
/// envelope larger than the whole budget takes the same lag path and is
/// delivered by the store resume (`read_page`'s at-least-one-envelope
/// guarantee), never buffered — no lag loop is possible because the resumed
/// head moves past it, and the per-attachment aggregate stays exact.
fn publish(
    attachments: &mut HashMap<AttachmentId, ActorAttachment>,
    envelopes: &[RawEnvelope],
    byte_budget: usize,
    metrics: &HubMetrics,
) {
    // Weighed once per envelope, not once per attachment.
    let weights = envelopes
        .iter()
        .map(envelope_weight_bytes)
        .collect::<Vec<_>>();
    let mut orphaned = Vec::new();
    for (attachment_id, attachment) in attachments.iter_mut() {
        if !attachment.active {
            continue;
        }
        for (envelope, weight) in envelopes.iter().zip(&weights) {
            let queued = attachment.queued_bytes.load(Ordering::Acquire);
            if queued.saturating_add(*weight) > byte_budget {
                metrics.catch_up_overflows.fetch_add(1, Ordering::Relaxed);
                let _ = attachment.lagged.send(Some(attachment.last_buffered_seq));
                attachment.active = false;
                break;
            }
            attachment.queued_bytes.fetch_add(*weight, Ordering::AcqRel);
            match attachment.events.try_send(QueuedEnvelope {
                weight: *weight,
                envelope: envelope.clone(),
            }) {
                Ok(()) => attachment.last_buffered_seq = envelope.seq,
                Err(_) => {
                    attachment.queued_bytes.fetch_sub(*weight, Ordering::AcqRel);
                    if attachment.events.is_closed() {
                        // The receiver is gone — a registration cancelled
                        // mid-flight or a dead replay task. Nobody can ever
                        // re-register this entry, so remove it instead of
                        // parking it lagged forever.
                        orphaned.push(attachment_id.clone());
                    } else {
                        metrics.catch_up_overflows.fetch_add(1, Ordering::Relaxed);
                        let _ = attachment.lagged.send(Some(attachment.last_buffered_seq));
                        attachment.active = false;
                    }
                    break;
                }
            }
        }
    }
    for attachment_id in orphaned {
        attachments.remove(&attachment_id);
    }
}
