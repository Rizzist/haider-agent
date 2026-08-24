//! CHARTER — the session actor: one session's serialized command order.
//!
//! What lives here: [`run_session_actor`] (every command arm) and the
//! synchronous [`publish`] fan-out it calls. Nothing else. What may NOT live
//! here: provider or tool work (never awaited in an arm — that is R1's
//! hub-actor purity rule; provider latency must never hold attach, menu, or
//! cancel liveness hostage), socket writes, RPC/transport concerns (rpc.rs),
//! and paced delivery (replay.rs). Awaits inside command arms are durable
//! store calls. Native-pipe projection is fed post-commit to an actor-owned
//! writer task and never delays publication; the journal remains its rebuild
//! authority. The loop's own `recv` is the other await.
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

const PIPE_SIDECAR_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

struct PipeSidecarTask {
    session_id: SessionId,
    writer: Arc<crate::pipe_native::PipeNativeWriter>,
    batches: Option<mpsc::UnboundedSender<Arc<[RawEnvelope]>>>,
    task: Option<JoinHandle<()>>,
}

impl PipeSidecarTask {
    fn spawn(
        writer: Arc<crate::pipe_native::PipeNativeWriter>,
        store: SqliteStoreHandle,
        session_id: SessionId,
    ) -> Self {
        let (batches, mut receiver) = mpsc::unbounded_channel::<Arc<[RawEnvelope]>>();
        let task_writer = Arc::clone(&writer);
        let task_session = session_id.clone();
        let task = tokio::spawn(async move {
            while let Some(envelopes) = receiver.recv().await {
                if let Err(error) = task_writer
                    .maintain(&store, &task_session, envelopes.as_ref())
                    .await
                {
                    tracing::warn!(
                        session_id = %task_session,
                        %error,
                        "native pipe sidecar maintenance failed; journal append remains committed"
                    );
                }
            }
        });
        Self {
            session_id,
            writer,
            batches: Some(batches),
            task: Some(task),
        }
    }

    fn enqueue(&self, envelopes: Arc<[RawEnvelope]>) {
        if self
            .batches
            .as_ref()
            .is_none_or(|batches| batches.send(envelopes).is_err())
        {
            self.writer.invalidate(&self.session_id);
        }
    }

    async fn finish(&mut self) {
        self.batches.take();
        let Some(mut task) = self.task.take() else {
            return;
        };
        match tokio::time::timeout(PIPE_SIDECAR_DRAIN_TIMEOUT, &mut task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.writer.invalidate(&self.session_id);
                tracing::warn!(
                    session_id = %self.session_id,
                    ?error,
                    "native pipe sidecar writer task failed; next open will reconcile"
                );
            }
            Err(_) => {
                task.abort();
                let _ = task.await;
                self.writer.invalidate(&self.session_id);
                tracing::warn!(
                    session_id = %self.session_id,
                    "native pipe sidecar writer drain timed out; next open will reconcile"
                );
            }
        }
    }
}

impl Drop for PipeSidecarTask {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            self.writer.invalidate(&self.session_id);
        }
    }
}

pub(super) fn selected_effect_recovery_action(
    menu: &Menu,
    answer: &DurableMenuAnswer,
) -> Option<EffectRecoveryAction> {
    let MenuKind::Recovery { option_actions, .. } = &menu.kind else {
        return None;
    };
    if let Some(action) = option_actions.get(answer.option_index as usize).copied() {
        return Some(action);
    }
    let key = answer.option_key.as_deref().or_else(|| {
        menu.options
            .get(answer.option_index as usize)
            .map(|option| option.key.as_str())
    })?;
    match key {
        "probe" => Some(EffectRecoveryAction::Probe),
        "mark_done" => Some(EffectRecoveryAction::MarkDone),
        "retry" => Some(EffectRecoveryAction::Retry),
        "abandon" => Some(EffectRecoveryAction::Abandon),
        _ => None,
    }
}

async fn settle_effect_recovery_via_committer(
    append_committer: &AppendCommitter,
    answer: &RawEnvelope,
    effect: &haider_protocol::ids::EffectId,
    action: EffectRecoveryAction,
    worker_generation: u64,
) -> Result<Vec<RawEnvelope>, HaiderError> {
    let envelopes = effect_recovery_envelopes(answer, effect, action, worker_generation)?;
    append_committer
        .commit(AppendCommitKind::General, envelopes)
        .await
}

fn effect_recovery_envelopes(
    answer: &RawEnvelope,
    effect: &haider_protocol::ids::EffectId,
    action: EffectRecoveryAction,
    worker_generation: u64,
) -> Result<Vec<RawEnvelope>, HaiderError> {
    let mut payloads = effect_recovery_payloads(effect, action)?;
    let mut envelopes = Vec::with_capacity(payloads.len());
    for (index, payload) in payloads.drain(..).enumerate() {
        let session_global = matches!(payload, EventPayload::SessionState(_));
        envelopes.push(haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: EventId::new(format!(
                "effect-resolution-{}-{}",
                answer.event_id,
                index + 1
            )),
            seq: 0,
            session_id: answer.session_id.clone(),
            branch_id: (!session_global)
                .then(|| answer.branch_id.clone())
                .flatten(),
            run_id: (!session_global).then(|| answer.run_id.clone()).flatten(),
            agent_id: (!session_global).then(|| answer.agent_id.clone()).flatten(),
            device_id: answer.device_id.clone(),
            authority_epoch: answer.authority_epoch,
            worker_generation,
            causation_id: Some(answer.event_id.clone()),
            correlation_id: answer.correlation_id.clone(),
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Pruned,
            },
            payload: serde_json::to_value(payload).map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("effect reconciliation payload could not serialize: {error}"),
                    false,
                )
            })?,
        });
    }
    Ok(envelopes)
}

#[cfg(test)]
async fn settle_effect_recovery(
    store: &SqliteStoreHandle,
    answer: &RawEnvelope,
    effect: &haider_protocol::ids::EffectId,
    action: EffectRecoveryAction,
    worker_generation: u64,
) -> Result<Vec<RawEnvelope>, HaiderError> {
    let mut envelopes = effect_recovery_envelopes(answer, effect, action, worker_generation)?;
    store.append(&mut envelopes).await?;
    Ok(envelopes)
}

fn effect_recovery_payloads(
    effect: &haider_protocol::ids::EffectId,
    action: EffectRecoveryAction,
) -> Result<Vec<EventPayload>, HaiderError> {
    let payloads = match action {
        EffectRecoveryAction::MarkDone => vec![
            EventPayload::Effect(haider_protocol::effect::EffectPhase::Outcome {
                effect: effect.clone(),
                outcome: haider_protocol::effect::EffectOutcome::Ok,
                freshness: None,
                workspace_mutation: None,
            }),
            EventPayload::RunState(RunState::Done),
            EventPayload::SessionState(haider_protocol::state::SessionState::Idle {
                interrupted: true,
            }),
        ],
        EffectRecoveryAction::Retry => vec![
            EventPayload::Effect(haider_protocol::effect::EffectPhase::Outcome {
                effect: effect.clone(),
                outcome: haider_protocol::effect::EffectOutcome::Failed {
                    error: "settled for explicit user retry".into(),
                },
                freshness: None,
                workspace_mutation: None,
            }),
            EventPayload::RunState(RunState::Errored),
            EventPayload::SessionState(haider_protocol::state::SessionState::Idle {
                interrupted: true,
            }),
        ],
        EffectRecoveryAction::Abandon => vec![
            EventPayload::Effect(haider_protocol::effect::EffectPhase::Outcome {
                effect: effect.clone(),
                outcome: haider_protocol::effect::EffectOutcome::Failed {
                    error: "abandoned by user after unknown outcome".into(),
                },
                freshness: None,
                workspace_mutation: None,
            }),
            EventPayload::RunFailed {
                code: ErrorCode::EffectUnknownOutcome,
                message: "effect was abandoned after its outcome became unknown".into(),
                retryable: false,
                presentation: Some(haider_protocol::error::ErrorPresentation::new(
                    "effect-outcome-abandoned",
                    "Unknown effect abandoned",
                    "The ambiguous effect was durably closed without retrying it.",
                    haider_protocol::error::ErrorScope::Tool,
                    [haider_protocol::error::ErrorAction::None],
                )),
            },
            EventPayload::RunState(RunState::Errored),
            EventPayload::SessionState(haider_protocol::state::SessionState::Idle {
                interrupted: true,
            }),
        ],
        EffectRecoveryAction::Probe => {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "effect probes must execute through the RPC probe handler",
                false,
            ));
        }
    };
    Ok(payloads)
}

// ──────────── session actor: the serialized command loop (§5.5) ─────────────

/// One session's entire command order, in one loop, in one task.
///
/// Both §5.5 invariants (module doc) hold by code shape here: awaits inside an
/// arm are durable store calls. Queue promotion's live reservation is a
/// synchronous mutex fence on the registered harness; it adds no actor-loop
/// await. Native pipe maintenance is only a nonblocking
/// post-commit wake; publication is a synchronous call after durable return
/// in the same arm, and the `Register` arm contains no await at all. Adding an
/// await between a store return and its `publish`, or anywhere in `Register`,
/// breaks a law — the forced-boundary tests in `tests/session_hub_tests.rs`
/// will catch it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_session_actor(
    session_id: SessionId,
    mut head: u64,
    mut authority_epoch: u64,
    worker_generation: u64,
    catch_up_byte_budget: usize,
    store: SqliteStoreHandle,
    append_committer: AppendCommitter,
    pipe_native: Arc<crate::pipe_native::PipeNativeWriter>,
    observer: Arc<dyn SessionHubObserver>,
    metrics: Arc<HubMetrics>,
    hooks: Arc<CommitProjection>,
    force_stop: Arc<AtomicBool>,
    mut commands: mpsc::Receiver<ActorCommand>,
) {
    let mut attachments = HashMap::<AttachmentId, ActorAttachment>::new();
    let mut pipe_sidecar =
        PipeSidecarTask::spawn(Arc::clone(&pipe_native), store.clone(), session_id.clone());
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
                envelopes,
                completed,
            } => {
                // INVARIANT 1 (module doc): the append is awaited here, and
                // `publish` below is synchronous in this same turn.
                let result = append_committer
                    .commit(AppendCommitKind::General, envelopes)
                    .await;
                match result {
                    Ok(committed) => {
                        let envelopes = Arc::<[RawEnvelope]>::from(committed);
                        head = envelopes.last().map_or(head, |envelope| envelope.seq);
                        if let Some(last) = envelopes.last() {
                            authority_epoch = last.authority_epoch;
                        }
                        observer.observe(HubObservation::Persisted {
                            session_id: session_id.clone(),
                            through_seq: head,
                        });
                        pipe_sidecar.enqueue(Arc::clone(&envelopes));
                        publish(
                            &mut attachments,
                            &envelopes,
                            catch_up_byte_budget,
                            &metrics,
                            &hooks,
                        );
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
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::ForkSession { command, completed } => {
                // Child metadata, copied history, boundary facts, audit, and
                // receipt are one store transaction. Only a fresh commit is
                // projected; a receipt replay remains response-only.
                let result = store.fork_session(command).await;
                if let Ok(SessionForkOutcome::Committed { envelopes, .. }) = &result {
                    if let Some(last) = envelopes.last() {
                        head = last.seq;
                        authority_epoch = last.authority_epoch;
                    }
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    pipe_sidecar.enqueue(Arc::from(envelopes.clone()));
                    publish_seeded_session(
                        &mut attachments,
                        envelopes,
                        catch_up_byte_budget,
                        &metrics,
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::CreateBranch { command, completed } => {
                // The registry row, BranchCreated fact, and R2 receipt are
                // one transaction. Only the committed fact is publishable.
                let result = store.create_branch(command).await;
                if let Ok(BranchCreateOutcome::Committed { envelope, .. }) = &result {
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
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::SelectModel { command, completed } => {
                // The metadata update, model_selected fact, and R2 receipt
                // are one transaction. Only the committed fact is
                // publishable, and the serialized arm is what makes "commit
                // here IS next-turn pickup" true: a turn accepted after this
                // arm re-reads the committed metadata.
                let result = store.select_session_model(command).await;
                if let Ok(SessionSelectModelOutcome::Committed { envelope, .. }) = &result {
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
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::Rename { command, completed } => {
                // G2: the metadata title update, session_renamed fact, and
                // R2 receipt are one transaction; only the committed fact
                // is publishable, and the serialized arm advances the
                // actor's in-memory head so the compaction head CAS keeps
                // observing journal truth (a store-direct rename desyncs
                // it). A Skipped auto-title moves nothing.
                let result = store.rename_session(command).await;
                if let Ok(haider_core::SessionRenameOutcome::Committed { envelope, .. }) = &result {
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
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::Seen { command, completed } => {
                // Attention state is durable session truth, not a surface
                // flag. Its non-meaningful config fact must still move this
                // actor's head so attachments and roster watchers converge.
                let result = store.mark_session_seen(command).await;
                if let Ok(haider_core::SessionSeenOutcome::Committed { envelope, .. }) = &result {
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
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::PinGraph {
                command,
                expected_digest,
                completed,
            } => {
                let result = match expected_digest {
                    Some(expected_digest) => {
                        store
                            .pin_graph_matching_digest(command, expected_digest)
                            .await
                    }
                    None => store.pin_graph(command).await,
                };
                let envelopes = match &result {
                    Ok(GraphPinOutcome::Committed { envelopes, .. }) => Some(envelopes.as_slice()),
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::AttachChildGraph { command, completed } => {
                let result = store.attach_child_graph(command).await;
                let envelopes = match &result {
                    Ok(ChildGraphAttachOutcome::Committed { envelopes, .. }) => {
                        Some(envelopes.as_slice())
                    }
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::ObserveChildTemplate { command, completed } => {
                let result = store.observe_child_template_success(command).await;
                let envelopes = match &result {
                    Ok(observed) if !observed.envelopes.is_empty() => {
                        Some(observed.envelopes.as_slice())
                    }
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::OpenGraphRunSet { command, completed } => {
                let result = store.open_graph_run_set(command).await;
                let envelopes = match &result {
                    Ok(GraphRunSetOpenOutcome::Committed { envelopes, .. }) => {
                        Some(envelopes.as_slice())
                    }
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::SwitchGraph { command, completed } => {
                let result = store.switch_graph(command).await;
                let envelopes = match &result {
                    Ok(GraphSwitchOutcome::Committed { envelopes, .. }) => {
                        Some(envelopes.as_slice())
                    }
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::AbandonGraph { command, completed } => {
                let result = store.abandon_graph(command).await;
                let envelopes = match &result {
                    Ok(GraphAbandonOutcome::Committed { envelopes, .. }) => {
                        Some(envelopes.as_slice())
                    }
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::RecordGraphEvidence { command, completed } => {
                let result = store.record_graph_evidence(command).await;
                let envelopes = match &result {
                    Ok(GraphEvidenceOutcome::Committed { envelopes, .. }) => {
                        Some(envelopes.as_slice())
                    }
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::RecordComputerEvidence { command, completed } => {
                let result = store.record_computer_evidence(command).await;
                let envelopes = match &result {
                    Ok(ComputerEvidenceOutcome::Committed { envelopes, .. }) => {
                        Some(envelopes.as_slice())
                    }
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::GuardGraphFinalization { command, completed } => {
                let result = store.guard_graph_finalization(command).await;
                let envelopes = match &result {
                    Ok(GraphFinalizationOutcome::Deferred { envelopes, .. })
                    | Ok(GraphFinalizationOutcome::ConfirmRequired { envelopes, .. }) => {
                        Some(envelopes.as_slice())
                    }
                    Ok(GraphFinalizationOutcome::AllowDone) | Err(_) => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::RecordProcessSignal { command, completed } => {
                let result = store.record_process_signal(command).await;
                let envelopes = match &result {
                    Ok(ProcessSignalOutcome::Committed { envelopes, .. }) => {
                        Some(envelopes.as_slice())
                    }
                    _ => None,
                };
                publish_graph_commit(
                    envelopes,
                    worker.as_ref(),
                    &session_id,
                    &mut head,
                    &mut authority_epoch,
                    &mut attachments,
                    catch_up_byte_budget,
                    &observer,
                    &metrics,
                    &hooks,
                );
                let _ = completed.send(result);
            }
            ActorCommand::SelectEffort { command, completed } => {
                // G3 clone of the SelectModel arm: metadata update,
                // effort_selected fact, and R2 receipt are one transaction;
                // the serialized arm keeps "commit here IS next-turn pickup"
                // true and advances the in-memory head with the fact (F3).
                let result = store.select_session_effort(command).await;
                if let Ok(SessionSelectEffortOutcome::Committed { envelope, .. }) = &result {
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
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::SelectAgentType { command, completed } => {
                // W-flow clone of the SelectEffort arm: registry-validated
                // metadata update, agent_type_selected fact, and R2 receipt
                // are one transaction; the serialized arm advances the
                // in-memory head with the fact (F3).
                let result = store.select_session_agent_type(command).await;
                if let Ok(SessionSelectAgentTypeOutcome::Committed { envelope, .. }) = &result {
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
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::SelectFast { command, completed } => {
                // G3 clone of the SelectModel arm for the fast toggle.
                let result = store.select_session_fast(command).await;
                if let Ok(SessionSelectFastOutcome::Committed { envelope, .. }) = &result {
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
                        &hooks,
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
                    pipe_sidecar.enqueue(Arc::from(envelopes.clone()));
                    publish(
                        &mut attachments,
                        envelopes,
                        catch_up_byte_budget,
                        &metrics,
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::QueueList { completed } => {
                let _ = completed.send(store.queue_snapshot(session_id.clone()).await);
            }
            ActorCommand::QueueRemove { command, completed } => {
                let result = store.queue_remove(command).await;
                if let Ok(outcome) = &result {
                    if let Some(last) = outcome.envelopes.last() {
                        head = last.seq;
                        authority_epoch = last.authority_epoch;
                    }
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    pipe_sidecar.enqueue(Arc::from(outcome.envelopes.clone()));
                    publish(
                        &mut attachments,
                        &outcome.envelopes,
                        catch_up_byte_budget,
                        &metrics,
                        &hooks,
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
            ActorCommand::QueuePromoteSteer {
                mut command,
                completed,
            } => {
                let result = match store.queue_promote_preview(command.clone()).await {
                    Ok(preview) => {
                        command.expected_active_run_id = Some(preview.active_run_id.clone());
                        if let Some(harness) =
                            worker.as_ref().and_then(|worker| worker.harness.as_ref())
                        {
                            match harness.reserve_promoted_steer(preview.text) {
                                Ok(reservation) => match store.queue_promote_steer(command).await {
                                    Ok(outcome) => {
                                        // Durable-before-delivery: only a
                                        // committed promotion releases the
                                        // core-side reservation. Dropping it
                                        // on every error leaves the queue row
                                        // and provider input untouched.
                                        reservation.commit().map(|()| (outcome, true))
                                    }
                                    Err(error) => Err(error),
                                },
                                Err(error) => Err(error),
                            }
                        } else {
                            // A durable active run can temporarily have no
                            // registered harness during recovery. Commit the
                            // same fenced delivery fact; the RPC layer hands
                            // it to the worker manager, and journal recovery
                            // retains the exact steer text if that handoff
                            // loses a process boundary.
                            store
                                .queue_promote_steer(command)
                                .await
                                .map(|outcome| (outcome, false))
                        }
                    }
                    Err(error) => Err(error),
                };
                if let Ok((outcome, _)) = &result {
                    if let Some(last) = outcome.envelopes.last() {
                        head = last.seq;
                        authority_epoch = last.authority_epoch;
                    }
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    pipe_sidecar.enqueue(Arc::from(outcome.envelopes.clone()));
                    publish(
                        &mut attachments,
                        &outcome.envelopes,
                        catch_up_byte_budget,
                        &metrics,
                        &hooks,
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
            ActorCommand::QueueConsume {
                lease_id,
                command,
                completed,
            } => {
                if worker.as_ref().map(|worker| &worker.lease_id) != Some(&lease_id) {
                    let _ = completed.send(Err(HaiderError::new(
                        ErrorCode::SingleWriterViolation,
                        "worker lease was superseded",
                        false,
                    )));
                    continue;
                }
                let result = store.queue_consume(command).await;
                if let Ok(Some(outcome)) = &result {
                    head = outcome.envelope.seq;
                    authority_epoch = outcome.envelope.authority_epoch;
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    pipe_sidecar.enqueue(Arc::from(vec![(*outcome.envelope).clone()]));
                    publish(
                        &mut attachments,
                        std::slice::from_ref(outcome.envelope.as_ref()),
                        catch_up_byte_budget,
                        &metrics,
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                }
                let _ = completed.send(result);
            }
            ActorCommand::AcceptRunRetry { command, completed } => {
                // MUTATION CHECK: moving the live-run gate out of the store
                // transaction lets two distinct retry command ids mint two
                // runs from one failed turn. Acceptance, receipt, and the
                // `Queued`/`RunRetried`/`ActiveRun` batch must stay atomic.
                let result = store.accept_run_retry(command).await;
                if let Ok(RunRetryOutcome::Committed { envelopes, .. }) = &result {
                    if let Some(last) = envelopes.last() {
                        head = last.seq;
                        authority_epoch = last.authority_epoch;
                    }
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    publish(
                        &mut attachments,
                        envelopes,
                        catch_up_byte_budget,
                        &metrics,
                        &hooks,
                    );
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
                    publish(
                        &mut attachments,
                        envelopes,
                        catch_up_byte_budget,
                        &metrics,
                        &hooks,
                    );
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
                        &hooks,
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
                envelopes,
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
                    // A compaction batch forks its node from the tree head
                    // captured at plan time. Session-config and Convergence
                    // Graph FACTS advance the journal without moving the
                    // conversation tree, so a delta made ONLY of those facts
                    // keeps the planned parent valid.
                    // Any other payload in the delta (run states, items,
                    // nodes, unknown kinds) still rejects: committing the
                    // summary would fork an obsolete tree parent.
                    if !non_tree_fact_only_delta(&store, &session_id, expected_head, head).await {
                        let _ = completed.send(Err(HaiderError::new(
                            ErrorCode::Busy,
                            format!(
                                "session history advanced from {expected_head} to {head} during compaction"
                            ),
                            true,
                        )));
                        continue;
                    }
                }
                // DURABLE TERMINAL TRUTH: lease identity is necessary but not
                // sufficient. This worker-only store transaction validates the
                // batch against the durable run head atomically with append.
                // Re-routing this to ordinary `append` must make the
                // cancel-before-Done mutation pin fail.
                let result = append_committer
                    .commit(AppendCommitKind::Worker, envelopes)
                    .await;
                match result {
                    Ok(committed) => {
                        let envelopes = Arc::<[RawEnvelope]>::from(committed);
                        head = envelopes.last().map_or(head, |envelope| envelope.seq);
                        if let Some(last) = envelopes.last() {
                            authority_epoch = last.authority_epoch;
                        }
                        observer.observe(HubObservation::Persisted {
                            session_id: session_id.clone(),
                            through_seq: head,
                        });
                        pipe_sidecar.enqueue(Arc::clone(&envelopes));
                        publish(
                            &mut attachments,
                            &envelopes,
                            catch_up_byte_budget,
                            &metrics,
                            &hooks,
                        );
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
                        &hooks,
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
                command.allow_prior_generation = command.allow_prior_generation
                    || recovered_menu.as_ref().is_some_and(|recovered| {
                        recovered.menu_id == command.answer.menu
                            && recovered.request_seq == command.request_seq
                            && recovered.opening_generation == command.worker_generation
                    });
                let answer = command.answer.clone();
                let mut outcome = store.resolve_menu(command).await;
                if let Ok(MenuResolutionOutcome::Committed {
                    ref envelope,
                    ref follow_up,
                    ref menu,
                }) = outcome
                {
                    head = follow_up.last().map_or(envelope.seq, |fact| fact.seq);
                    authority_epoch = follow_up
                        .last()
                        .map_or(envelope.authority_epoch, |fact| fact.authority_epoch);
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    let mut published = Vec::with_capacity(1 + follow_up.len());
                    published.push(envelope.as_ref().clone());
                    published.extend(follow_up.iter().cloned());
                    publish(
                        &mut attachments,
                        &published,
                        catch_up_byte_budget,
                        &metrics,
                        &hooks,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    if !matches!(menu.kind, MenuKind::GraphHumanConfirm { .. })
                        && let Some(harness) =
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
                if let Ok(MenuResolutionOutcome::Committed { envelope, menu, .. }) = &outcome
                    && let MenuKind::Recovery { effect, .. } = &menu.kind
                    && let Some(action) = selected_effect_recovery_action(menu, &answer)
                    && action != EffectRecoveryAction::Probe
                {
                    match settle_effect_recovery_via_committer(
                        &append_committer,
                        envelope,
                        effect,
                        action,
                        worker_generation,
                    )
                    .await
                    {
                        Ok(envelopes) => {
                            if let Some(last) = envelopes.last() {
                                head = last.seq;
                                authority_epoch = last.authority_epoch;
                            }
                            pipe_sidecar.enqueue(Arc::from(envelopes.clone()));
                            publish(
                                &mut attachments,
                                &envelopes,
                                catch_up_byte_budget,
                                &metrics,
                                &hooks,
                            );
                        }
                        Err(error) => {
                            tracing::error!(
                                session_id = %session_id,
                                effect = %effect,
                                ?error,
                                "committed effect recovery answer could not settle"
                            );
                            // The menu answer is durable, so callers must not
                            // be told the reconciliation itself succeeded.
                            // Startup/live reduction consumes MenuAnswered
                            // and reopens the card if these follow-ups are
                            // missing.
                            outcome = Err(error);
                        }
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
            ActorCommand::StopIfQuiescent { completed } => {
                let result = if attachments.is_empty() {
                    session_is_quiescent(&store, &session_id).await
                } else {
                    Ok(false)
                };
                let should_stop = result.as_ref().is_ok_and(|quiescent| *quiescent);
                if should_stop {
                    pipe_sidecar.finish().await;
                    let _ = completed.send(result);
                    return;
                }
                let _ = completed.send(result);
            }
            ActorCommand::Stop => break,
        }
    }
    pipe_sidecar.finish().await;
}

#[allow(clippy::too_many_arguments)]
fn publish_graph_commit(
    envelopes: Option<&[RawEnvelope]>,
    worker: Option<&RegisteredWorker>,
    session_id: &SessionId,
    head: &mut u64,
    authority_epoch: &mut u64,
    attachments: &mut HashMap<AttachmentId, ActorAttachment>,
    catch_up_byte_budget: usize,
    observer: &Arc<dyn SessionHubObserver>,
    metrics: &HubMetrics,
    hooks: &Arc<CommitProjection>,
) {
    let Some(envelopes) = envelopes else {
        return;
    };
    if let Some(last) = envelopes.last() {
        *head = last.seq;
        *authority_epoch = last.authority_epoch;
    }
    observer.observe(HubObservation::Persisted {
        session_id: session_id.clone(),
        through_seq: *head,
    });
    publish(attachments, envelopes, catch_up_byte_budget, metrics, hooks);
    if let Some(harness) = worker.and_then(|worker| worker.harness.as_ref())
        && let Some(RunState::InputRequired { menu: waiting }) = harness.current_state()
    {
        for envelope in envelopes {
            let closes_waiting = serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| {
                    matches!(payload, EventPayload::MenuClosed { menu, .. } if menu == waiting)
                });
            if closes_waiting {
                if let Err(error) = harness.apply_committed_menu_event(envelope.clone()) {
                    tracing::warn!(
                        session_id = %session_id,
                        error = ?error,
                        "committed graph menu closure could not wake the live harness"
                    );
                }
                break;
            }
        }
    }
    observer.observe(HubObservation::Published {
        session_id: session_id.clone(),
        through_seq: *head,
    });
}

async fn session_is_quiescent(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
) -> Result<bool, HaiderError> {
    let mut cursor = 0;
    let mut states = HashMap::<RunId, RunState>::new();
    loop {
        let page = store.read(session_id, cursor, 256).await?;
        if page.is_empty() {
            return Ok(states.values().all(RunState::is_terminal));
        }
        cursor = page.last().map_or(cursor, |event| event.seq);
        for event in page {
            let Some(run_id) = event.run_id else {
                continue;
            };
            if let Ok(EventPayload::RunState(state)) =
                serde_json::from_value::<EventPayload>(event.payload)
            {
                states.insert(run_id, state);
            }
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
    hooks: &Arc<CommitProjection>,
) {
    hooks.observe_committed(envelopes);
    publish_attachments(attachments, envelopes, byte_budget, metrics);
}

/// Publishes a newly seeded session without replaying copied parent facts into
/// hook execution. The final fork audit is the only newly-originated fact;
/// observe/roster caches seeing its non-one sequence rebuild from child truth.
fn publish_seeded_session(
    attachments: &mut HashMap<AttachmentId, ActorAttachment>,
    envelopes: &[RawEnvelope],
    byte_budget: usize,
    metrics: &HubMetrics,
    hooks: &Arc<CommitProjection>,
) {
    if let Some(audit) = envelopes.last() {
        hooks.observe_committed(std::slice::from_ref(audit));
    }
    publish_attachments(attachments, envelopes, byte_budget, metrics);
}

fn publish_attachments(
    attachments: &mut HashMap<AttachmentId, ActorAttachment>,
    envelopes: &[RawEnvelope],
    byte_budget: usize,
    metrics: &HubMetrics,
) {
    if !attachments.values().any(|attachment| attachment.active) {
        return;
    }
    // Weighed once per envelope, not once per attachment.
    let weights = envelopes
        .iter()
        .map(envelope_weight_bytes)
        .collect::<Vec<_>>();
    // One durable envelope clone per publication, shared across every
    // attachment lane. The replay task encodes it by reference.
    let shared = envelopes.iter().cloned().map(Arc::new).collect::<Vec<_>>();
    let mut orphaned = Vec::new();
    for (attachment_id, attachment) in attachments.iter_mut() {
        if !attachment.active {
            continue;
        }
        for ((envelope, weight), shared) in envelopes.iter().zip(&weights).zip(&shared) {
            let queued = attachment.queued_bytes.load(Ordering::Acquire);
            if queued.saturating_add(*weight) > byte_budget {
                metrics.catch_up_overflows.fetch_add(1, Ordering::Relaxed);
                attachment.active = false;
                if attachment
                    .lagged
                    .send(Some(attachment.last_buffered_seq))
                    .is_err()
                {
                    orphaned.push(attachment_id.clone());
                }
                break;
            }
            attachment.queued_bytes.fetch_add(*weight, Ordering::AcqRel);
            match attachment.events.try_send(QueuedEnvelope {
                weight: *weight,
                envelope: Arc::clone(shared),
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
                        attachment.active = false;
                        if attachment
                            .lagged
                            .send(Some(attachment.last_buffered_seq))
                            .is_err()
                        {
                            orphaned.push(attachment_id.clone());
                        }
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod discarded_result_tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum LagTrigger {
        ByteBudgetOverflow,
        EventChannelGap,
    }

    fn lag_test_envelope() -> RawEnvelope {
        haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: EventId::new("discarded-result-lag"),
            seq: 1,
            session_id: SessionId::new("discarded-result-session"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("discarded-result-device"),
            authority_epoch: 0,
            worker_generation: 0,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: serde_json::Value::Null,
        }
    }

    fn exercise_lag_transition(
        trigger: LagTrigger,
        keep_lag_receiver: bool,
    ) -> (Option<bool>, Option<u64>) {
        let attachment_id = AttachmentId::new("discarded-result-attachment");
        let envelope = lag_test_envelope();
        let (events, _event_receiver) = mpsc::channel(1);
        if matches!(trigger, LagTrigger::EventChannelGap) {
            events
                .try_send(QueuedEnvelope {
                    weight: 1,
                    envelope: Arc::new(envelope.clone()),
                })
                .unwrap();
        }
        let (lagged, lag_receiver) = watch::channel(Option::<u64>::None);
        let lag_receiver = keep_lag_receiver.then_some(lag_receiver);
        let mut attachments = HashMap::from([(
            attachment_id.clone(),
            ActorAttachment {
                events,
                lagged,
                queued_bytes: Arc::new(AtomicUsize::new(0)),
                last_buffered_seq: 0,
                active: true,
            },
        )]);
        let byte_budget = match trigger {
            LagTrigger::ByteBudgetOverflow => 0,
            LagTrigger::EventChannelGap => usize::MAX,
        };

        publish_attachments(
            &mut attachments,
            &[envelope],
            byte_budget,
            &HubMetrics::default(),
        );

        let active = attachments
            .get(&attachment_id)
            .map(|attachment| attachment.active);
        let published = lag_receiver.and_then(|receiver| *receiver.borrow());
        (active, published)
    }

    /// MUTATION CHECK: restore both lag publications to discarded
    /// `let _ = ...send(...)` calls. Expected failure: each trigger leaves
    /// its receiver-less attachment present as inactive/current actor state.
    #[test]
    fn failed_lag_publication_removes_the_orphaned_attachment() {
        for trigger in [LagTrigger::ByteBudgetOverflow, LagTrigger::EventChannelGap] {
            let (active, published) = exercise_lag_transition(trigger, false);
            assert_eq!(
                active, None,
                "{trigger:?} must remove an attachment that cannot receive its only lag transition"
            );
            assert_eq!(published, None);
        }
    }

    /// MUTATION CHECK: make receiver cleanup unconditional after lag
    /// publication. Expected failure: each healthy attachment disappears
    /// instead of remaining inactive for store-backed re-registration.
    #[test]
    fn successful_lag_publication_keeps_the_attachment_for_reregistration() {
        for trigger in [LagTrigger::ByteBudgetOverflow, LagTrigger::EventChannelGap] {
            let (active, published) = exercise_lag_transition(trigger, true);
            assert_eq!(
                active,
                Some(false),
                "{trigger:?} must retain the healthy attachment on the existing lag path"
            );
            assert_eq!(
                published,
                Some(0),
                "{trigger:?} must publish the lag cursor"
            );
        }
    }
}

/// True when every envelope in `(expected_head, head]` is a recognized fact
/// that cannot move the conversation tree. This includes session config and
/// Convergence Graph lifecycle/evidence/menu facts. Bounded scan; any read
/// failure, coverage gap, run/item/tree payload, or unknown kind keeps the
/// honest Busy rejection. The one await is a store call (§5.5 actor charter).
async fn non_tree_fact_only_delta(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    expected_head: u64,
    head: u64,
) -> bool {
    /// A compaction interleave is a handful of facts; anything larger is not
    /// a metadata blip and keeps the rejection without an unbounded scan.
    const MAX_TOLERATED_DELTA: u64 = 64;
    if head <= expected_head || head - expected_head > MAX_TOLERATED_DELTA {
        return false;
    }
    let expected_len = head - expected_head;
    let Ok(delta) = StoreHandle::read(
        store,
        session_id,
        expected_head,
        usize::try_from(expected_len)
            .unwrap_or(usize::MAX)
            .saturating_add(1),
    )
    .await
    else {
        return false;
    };
    delta.len() as u64 == expected_len
        && delta
            .first()
            .is_some_and(|envelope| envelope.seq > expected_head)
        && delta.last().is_some_and(|envelope| envelope.seq == head)
        && delta
            .iter()
            .all(|envelope| payload_preserves_conversation_tree(&envelope.payload))
}

fn payload_preserves_conversation_tree(payload: &serde_json::Value) -> bool {
    if haider_protocol::retry::RunRetryEventPayload::from_payload_value(payload.clone()).is_ok() {
        return true;
    }
    if serde_json::from_value::<haider_protocol::session::SessionConfigEventPayload>(
        payload.clone(),
    )
    .is_ok()
    {
        return true;
    }
    matches!(
        serde_json::from_value::<EventPayload>(payload.clone()),
        Ok(EventPayload::GraphPinned(_)
            | EventPayload::GraphRunSetOpened(_)
            | EventPayload::TodoGraphAttached(_)
            | EventPayload::ChildGraphAttached(_)
            | EventPayload::ChildTemplateObserved(_)
            | EventPayload::ChildTemplatePromoted(_)
            | EventPayload::GraphAttemptOpened(_)
            | EventPayload::EvidenceRecorded(_)
            | EventPayload::GraphGateSatisfied(_)
            | EventPayload::GraphAdvanced(_)
            | EventPayload::GraphNodeReadied(_)
            | EventPayload::GraphBlocked(_)
            | EventPayload::GraphCompleted(_)
            | EventPayload::GraphAbandoned(_)
            | EventPayload::GraphSuperseded(_)
            | EventPayload::GraphFinalizationDeferred(_)
            | EventPayload::ProcessSignalRecorded(_)
            | EventPayload::MenuOpened(Menu {
                kind: MenuKind::GraphHumanConfirm { .. } | MenuKind::GraphAbandonConfirm { .. },
                ..
            })
            | EventPayload::MenuAnswered(_)
            | EventPayload::MenuClosed { .. })
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod error_wave3_tests {
    use super::*;
    use haider_protocol::effect::{EffectOutcome, EffectPhase};

    #[test]
    fn e6a_effect_recovery_handlers_produce_typed_durable_closure() {
        let effect = haider_protocol::ids::EffectId::new("effect-e6a-handlers");

        let mark_done =
            effect_recovery_payloads(&effect, EffectRecoveryAction::MarkDone).expect("mark done");
        assert!(matches!(
            &mark_done[0],
            EventPayload::Effect(EffectPhase::Outcome {
                outcome: EffectOutcome::Ok,
                ..
            })
        ));
        assert!(matches!(
            mark_done[1],
            EventPayload::RunState(RunState::Done)
        ));

        let retry = effect_recovery_payloads(&effect, EffectRecoveryAction::Retry).expect("retry");
        assert!(matches!(
            &retry[0],
            EventPayload::Effect(EffectPhase::Outcome {
                outcome: EffectOutcome::Failed { .. },
                ..
            })
        ));
        assert!(matches!(
            retry[1],
            EventPayload::RunState(RunState::Errored)
        ));

        let abandon =
            effect_recovery_payloads(&effect, EffectRecoveryAction::Abandon).expect("abandon");
        assert!(matches!(
            &abandon[1],
            EventPayload::RunFailed {
                code: ErrorCode::EffectUnknownOutcome,
                presentation: Some(presentation),
                ..
            } if presentation.subcode.as_str() == "effect-outcome-abandoned"
        ));
        assert!(matches!(
            abandon[2],
            EventPayload::RunState(RunState::Errored)
        ));

        assert!(
            effect_recovery_payloads(&effect, EffectRecoveryAction::Probe).is_err(),
            "probe is a real RPC check, never a fake durable settlement"
        );
    }

    #[tokio::test]
    async fn e6a_abandon_closes_the_unknown_effect_durably() {
        let root = tempfile::tempdir().expect("profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        let session_id = SessionId::new("e6a-abandon-session");
        store
            .create_session(haider_store::SessionCreateCommand {
                command_id: "create-e6a-abandon".into(),
                request_digest: "digest-e6a-abandon".into(),
                request_json: r#"{"session":"e6a-abandon"}"#.into(),
                session_id: session_id.clone(),
                cwd: root.path().to_string_lossy().into_owned(),
                provider: "fake".into(),
                model: "fake-v1".into(),
                max_tokens: 1_024,
                permission_overrides: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                system_prompt_version: "test-system-v1".into(),
                event_id: EventId::new("created-e6a-abandon"),
                device_id: DeviceId::new("e6a-device"),
            })
            .await
            .expect("create session");
        let answer = haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: EventId::new("answered-e6a-abandon"),
            seq: 2,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: Some(RunId::new("run-e6a-abandon")),
            agent_id: None,
            device_id: DeviceId::new("e6a-device"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Pruned,
            },
            payload: serde_json::to_value(EventPayload::IdleDecayed).expect("answer payload"),
        };
        settle_effect_recovery(
            &store,
            &answer,
            &haider_protocol::ids::EffectId::new("effect-e6a-abandon"),
            EffectRecoveryAction::Abandon,
            store.worker_generation(),
        )
        .await
        .expect("durable abandon");

        let events = store.read(&session_id, 0, 100).await.expect("read journal");
        assert!(events.iter().any(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::RunFailed {
                    code: ErrorCode::EffectUnknownOutcome,
                    ..
                })
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::RunState(RunState::Errored))
            )
        }));
        store.close().await.expect("close");
    }
}
