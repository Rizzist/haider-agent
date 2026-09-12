//! Non-destructive native close. The caller owns the sole Control attachment;
//! a close is refused while other clients or durable work still need the session.
//! Detached RPC callers cannot cancel an admitted resource-release barrier.

use super::*;

struct CloseFence {
    hub: SessionHub,
    session_id: SessionId,
    actor: SessionActorHandle,
}

impl Drop for CloseFence {
    fn drop(&mut self) {
        self.actor.closing.store(false, Ordering::Release);
        if let Ok(mut closing) = self.hub.inner.closing_sessions.lock() {
            closing.remove(&self.session_id);
        }
    }
}

fn close_error(message: &str) -> HaiderError {
    HaiderError::new(ErrorCode::Internal, message, true)
}

fn busy(message: &str) -> HaiderError {
    HaiderError::new(ErrorCode::Busy, message, true)
}

impl SessionHub {
    fn session_activity_lock(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<tokio::sync::RwLock<()>>, HaiderError> {
        let mut locks = lock(&self.inner.session_activity).map_err(hub_error_as_store)?;
        locks.retain(|_, activity| activity.strong_count() != 0);
        if let Some(activity) = locks.get(session_id).and_then(Weak::upgrade) {
            return Ok(activity);
        }
        let activity = Arc::new(tokio::sync::RwLock::new(()));
        locks.insert(session_id.clone(), Arc::downgrade(&activity));
        Ok(activity)
    }

    pub(crate) fn try_session_activity(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<tokio::sync::OwnedRwLockReadGuard<()>>, HaiderError> {
        self.session_activity_lock(session_id)?
            .try_read_owned()
            .map(Arc::new)
            .map_err(|_| busy("session close has fenced resource activity"))
    }

    pub(crate) async fn session_activity(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<tokio::sync::OwnedRwLockReadGuard<()>>, HaiderError> {
        Ok(Arc::new(
            self.session_activity_lock(session_id)?.read_owned().await,
        ))
    }

    pub(super) async fn join_idle_release(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HaiderError> {
        let idle = lock(&self.inner.idle_release_tasks)
            .map_err(hub_error_as_store)?
            .remove(session_id);
        if let Some(idle) = idle {
            idle.head.send_replace(None);
            idle.task
                .await
                .map_err(|_| close_error("idle release task failed during session teardown"))?;
        }
        Ok(())
    }

    pub(super) async fn close_attachment_session(
        &self,
        attachment_id: AttachmentId,
        connection_id: String,
        sink: Arc<dyn FrameSink>,
    ) -> Result<SessionId, HaiderError> {
        let (completed, response) = oneshot::channel();
        {
            let mut tasks = lock(&self.inner.session_close_tasks).map_err(hub_error_as_store)?;
            if self.inner.draining.load(Ordering::Acquire)
                || self.inner.close_admission_stopped.load(Ordering::Acquire)
            {
                return Err(busy("daemon is draining"));
            }
            tasks.retain(|task| !task.is_finished());
            // RPC cancellation must not leave an unbounded detached queue.
            // Each admitted close holds at most one store operation at a time.
            if tasks.len() >= 16 {
                return Err(busy("native session close admission is full; retry"));
            }
            let hub = self.clone();
            tasks.push(tokio::spawn(async move {
                let result = hub
                    .close_attached_session(&attachment_id, &connection_id, &sink)
                    .await;
                let _ = completed.send(result);
            }));
        }
        response
            .await
            .map_err(|_| close_error("session close task stopped before acknowledgement"))?
    }

    async fn close_attached_session(
        &self,
        attachment_id: &AttachmentId,
        connection_id: &str,
        sink: &Arc<dyn FrameSink>,
    ) -> Result<SessionId, HaiderError> {
        let (session_id, actor) = {
            let owners = lock(&self.inner.attachments).map_err(hub_error_as_store)?;
            let owner = owners
                .get(attachment_id)
                .filter(|owner| {
                    owner.connection_id == connection_id && owner.mode == AttachMode::Control
                })
                .ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::InvalidArgument,
                        "native close requires this connection's Control attachment",
                        false,
                    )
                })?;
            (owner.session_id.clone(), owner.actor.clone())
        };
        // Retain the SAME serial until completion. Removing this map entry
        // would let waiters on the old lock race a newly created lock on reopen.
        let _admission = self.lock_workflow_selection(&session_id).await;
        // Work admission uses this same serial. Refuse an active run before
        // taking the resource fence so a refused close cannot interrupt its
        // inline task-adoption call. Actor FIFO fences recheck quiescence.
        if self.session_has_nonterminal_runs(&session_id).await? {
            return Err(busy("session has active work; retry after it settles"));
        }
        let _activity_fence = self
            .session_activity_lock(&session_id)?
            .try_write_owned()
            .map_err(|_| {
                busy("session has live hook execution, subscriptions, or task recovery; retry")
            })?;
        {
            let deleting = lock(&self.inner.deleting_sessions).map_err(hub_error_as_store)?;
            let mut closing = lock(&self.inner.closing_sessions).map_err(hub_error_as_store)?;
            if deleting.contains(&session_id) || closing.contains(&session_id) {
                return Err(busy("session teardown is already in progress"));
            }
            let owners = lock(&self.inner.attachments).map_err(hub_error_as_store)?;
            if owners
                .get(attachment_id)
                .is_none_or(|owner| owner.connection_id != connection_id)
                || owners
                    .iter()
                    .any(|(id, owner)| id != attachment_id && owner.session_id == session_id)
                || lock(&self.inner.descendant_attachments)
                    .map_err(hub_error_as_store)?
                    .values()
                    .any(|owner| owner.session_ids.contains(&session_id))
            {
                return Err(busy("native close requires the session's sole attachment"));
            }
            closing.insert(session_id.clone());
        }
        let _fence = CloseFence {
            hub: self.clone(),
            session_id: session_id.clone(),
            actor: actor.clone(),
        };
        if self.inner.tasks.running_count(&session_id) != 0
            || self.inner.monitors.has_session_resources(&session_id)
        {
            return Err(busy(
                "session has active work or monitors; retry after it settles",
            ));
        }
        self.inner
            .monitors
            .join_session_tasks(&session_id)
            .await
            .map_err(|error| close_error(&error.to_string()))?;
        let hooks = lock(&self.inner.hooks)
            .map_err(hub_error_as_store)?
            .as_ref()
            .map(|hooks| {
                hooks
                    .upgrade()
                    .ok_or_else(|| busy("hook engine is unavailable; retry session close"))
            })
            .transpose()?;
        if let Some(hooks) = &hooks
            && !hooks.permits_native_close(&session_id).await?
        {
            return Err(busy("session has configured hook dispatch pending; retry"));
        }

        let owner = self
            .take_attachment(attachment_id, Some(connection_id))
            .map_err(hub_error_as_store)?
            .ok_or_else(|| busy("attachment was detached during close"))?;
        let _ = sink.purge_attachment(attachment_id);
        Self::finish_detach(attachment_id, owner).await;
        let replays = {
            let mut tasks = lock(&self.inner.replay_tasks).map_err(hub_error_as_store)?;
            let ids = tasks
                .iter()
                .filter(|(_, replay)| replay.sessions.contains(&session_id))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| tasks.remove(&id).map(|replay| replay.task))
                .collect::<Vec<_>>()
        };
        let mut replay_failed = false;
        for replay in replays {
            replay_failed |= replay.await.is_err();
        }
        if replay_failed {
            return Err(close_error("attachment replay failed during close"));
        }
        // FIFO fence consumes any pre-close command already executing when
        // the atomic admission fence was installed, including durable commits.
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::FenceIfQuiescent { completed })
            .await
            .map_err(|_| close_error("session actor stopped before close fence"))?;
        if !response
            .await
            .map_err(|_| close_error("session actor lost close fence reply"))??
        {
            return Err(busy("session became nonterminal during close"));
        }
        let workers = lock(&self.inner.worker_manager)
            .map_err(hub_error_as_store)?
            .clone();
        if let Some(workers) = workers {
            workers.retire(session_id.clone()).await?;
        }
        self.inner.tasks.join_session(&session_id).await?;
        if let Some(hooks) = &hooks
            && !hooks.permits_native_close(&session_id).await?
        {
            return Err(busy(
                "worker retirement left configured hook dispatch pending; reattach and retry",
            ));
        }
        // A terminal run may still be committing its final session-idle
        // suffix. Retire/join the supervisor BEFORE rejecting worker writes.
        // Now seal the FIFO against senders obtained before close admission.
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::FenceForClose { completed })
            .await
            .map_err(|_| close_error("session actor stopped before final close fence"))?;
        if !response
            .await
            .map_err(|_| close_error("session actor lost final close fence reply"))??
        {
            return Err(busy("session became nonterminal during worker retirement"));
        }
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::StopForClose { completed })
            .await
            .map_err(|_| close_error("session actor stopped before close drain"))?;
        let drain = response
            .await
            .map_err(|_| close_error("session actor lost close drain reply"))?;
        let task = lock(&self.inner.session_actor_tasks)
            .map_err(hub_error_as_store)?
            .remove(&session_id)
            .ok_or_else(|| close_error("session actor join handle missing"))?;
        let joined = task.await;
        lock(&self.inner.actors)
            .map_err(hub_error_as_store)?
            .remove(&session_id);
        joined.map_err(|_| close_error("session actor failed during close"))?;
        drain?;
        let peer = lock(&self.inner.peer_service)
            .map_err(hub_error_as_store)?
            .clone();
        if let Some(peer) = peer {
            peer.close_session(&session_id)
                .await
                .map_err(|error| close_error(&error.to_string()))?;
        }
        self.join_idle_release(&session_id).await?;
        self.inner.pipe_native.release_clean(&session_id);
        self.inner
            .prompt_history
            .evict_session_bodies(&session_id)
            .await;
        self.inner
            .turn_setup_reductions
            .remove_session(&session_id)
            .await;
        self.inner.observe_digests.remove(&session_id);
        self.inner.tasks.release_terminal_session(&session_id);
        // The SQLite connection and HTTP transports are profile services.
        // Their lifetimes deliberately do not end at a session close.
        tracing::debug!(%session_id, "native session close joined worker, replay, actor and sidecar");
        Ok(session_id)
    }
}
