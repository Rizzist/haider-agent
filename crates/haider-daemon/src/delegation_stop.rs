//! Subagent stop semantics (owner decision 2026-09-25, 973-LANES.md).
//!
//! * Stopping the main thread (TUI Esc, `turn.cancel`, `run --stop-main`)
//!   stops ONLY the main thread. Its subagents keep running. A foreground
//!   subagent the stopped turn was waiting on is detached: it continues in
//!   the background and its result is delivered to the parent session when
//!   it finishes (steered into an active parent run, otherwise journalled as
//!   a prompt-visible note that never starts provider work by itself). A
//!   durable "N subagents still running" marker records what the stop left.
//! * Cancelling one named subagent stops that subagent only: its original
//!   run plus any later active run, and its own monitors. Its children keep
//!   running.
//! * Stop-all (`run --stop`, `run --stop-subagents`, `agent cancel --all`)
//!   walks the delegation tree by SESSION, so children spawned in later runs
//!   (messaged `run-child-message-*` runs, monitor-woken subturns) are
//!   reached. It is unbounded in depth, cycle-safe through a seen-set, removes
//!   every descendant session's monitors. Public operations durably claim
//!   exact run and monitor incarnations; replay never retargets later work.

use super::*;
use crate::monitor_stop_fence::MonitorStopFence;
use haider_protocol::subagent_stop::{
    DETACHED_RESULT_SUMMARY_MAX_BYTES, DetachedChildResultV1, DetachedChildStateV1,
    DetachedDeliveryPhaseV1, DetachedDeliveryV1, DetachedReasonV1, STILL_RUNNING_AGENT_LIST_MAX,
    StillRunningAgentV1, SubagentsStillRunningV1, bounded_summary,
};
use std::sync::LazyLock;

const DETACHED_DELIVERY_POLL: Duration = Duration::from_millis(250);
/// A stop waits at most this long for its cancelled runs to settle before
/// reporting (the report stays truthful either way).
const STOP_CONVERGENCE_BOUND: Duration = Duration::from_secs(10);
const STOP_CONVERGENCE_RESUBMIT_AFTER: Duration = Duration::from_millis(750);
const STOP_CONVERGENCE_POLL: Duration = Duration::from_millis(50);

/// In-process guard so one detached child has at most one delivery watcher.
/// Durable dedupe is the deterministic delivery event id.
static DETACHED_WATCHERS: LazyLock<std::sync::Mutex<HashSet<AgentId>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

#[cfg(all(test, unix))]
struct StopDrainPause {
    arrived: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(all(test, unix))]
static STOP_DRAIN_PAUSES: LazyLock<
    std::sync::Mutex<HashMap<(SessionId, &'static str), StopDrainPause>>,
> = LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Test-only ordering seam after the first monitor listing has drained. It
/// lets a client registration commit before the production final drain.
#[cfg(all(test, unix))]
pub(crate) fn pause_after_first_drain(
    session: &SessionId,
    scope: &'static str,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    STOP_DRAIN_PAUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            (session.clone(), scope),
            StopDrainPause {
                arrived: arrived_tx,
                resume: resume_rx,
            },
        );
    (arrived_rx, resume_tx)
}

#[cfg(all(test, unix))]
async fn first_drain_test_pause(session: &SessionId, scope: &'static str) {
    let pause = STOP_DRAIN_PAUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(session.clone(), scope));
    if let Some(pause) = pause {
        let _ = pause.arrived.send(());
        let _ = pause.resume.await;
    }
}

/// Where a stop-all walk starts.
#[derive(Clone)]
pub(crate) enum SubagentTreeRoot {
    /// Every child spawned by one run (its tree).
    Run {
        session_id: SessionId,
        run_id: RunId,
    },
    /// Every child of one session, from any of its runs.
    Session(SessionId),
}

/// Outcome of a stop-all walk or a main-only detach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SubagentStopSummary {
    pub(crate) cancelled_runs: u64,
    pub(crate) sessions: u64,
    pub(crate) monitors_removed: u64,
    pub(crate) still_running: u64,
}

impl SubagentStopSummary {
    pub(crate) fn wire(self) -> haider_rpc::SubagentStopSummaryWire {
        haider_rpc::SubagentStopSummaryWire {
            cancelled_runs: self.cancelled_runs,
            sessions: self.sessions,
            monitors_removed: self.monitors_removed,
            still_running: self.still_running,
        }
    }
}

fn is_background_record(record: &DelegationRecord) -> bool {
    record
        .manifest
        .coordinates
        .as_ref()
        .and_then(|coordinates| coordinates.get("background"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StopIncarnation {
    pub(crate) session_id: SessionId,
    pub(crate) runs: Vec<RunId>,
    pub(crate) monitors: Vec<String>,
}

impl DelegationHandle {
    pub(crate) async fn stop_incarnations(
        &self,
        root: &SubagentTreeRoot,
        admission: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<Vec<StopIncarnation>, HaiderError> {
        // Root run IDs are causal anchors, not cancellation targets. A child
        // registered by one of these runs while the stop is in flight belongs
        // to this intent; a child registered by a later run does not.
        let mut anchor = match root {
            SubagentTreeRoot::Run { session_id, run_id } => StopIncarnation {
                session_id: session_id.clone(),
                runs: vec![run_id.clone()],
                monitors: Vec::new(),
            },
            SubagentTreeRoot::Session(session_id) => {
                self.session_stop_incarnation(session_id, admission).await?
            }
        };
        anchor.monitors.clear();
        let mut targets = vec![anchor];
        for record in self.walk_tree(root).await? {
            targets.push(self.delegation_stop_incarnation(&record, admission).await?);
        }
        Ok(targets)
    }

    pub(crate) async fn delegation_stop_incarnation(
        &self,
        record: &DelegationRecord,
        admission: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<StopIncarnation, HaiderError> {
        let mut target = self
            .session_stop_incarnation(&record.child_session_id, admission)
            .await?;
        // Registration precedes child turn acceptance. That stable original
        // run belongs to the intent even before it has a journal head.
        if !target.runs.contains(&record.child_run_id) {
            target.runs.push(record.child_run_id.clone());
            target.runs.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        }
        Ok(target)
    }

    pub(crate) async fn session_stop_incarnation(
        &self,
        session: &SessionId,
        admission: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<StopIncarnation, HaiderError> {
        let mut runs = self
            .child_session_snapshot(session)
            .await?
            .states
            .into_keys()
            .collect::<Vec<_>>();
        runs.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let monitors = self
            .hub
            .inner_monitor()
            .stop_monitor_ids(&self.hub, session, admission)
            .await
            .map_err(|error| HaiderError::new(ErrorCode::Internal, error.to_string(), true))?;
        Ok(StopIncarnation {
            session_id: session.clone(),
            runs,
            monitors,
        })
    }

    /// Nonterminal runs of a session that a cancel can still move: every run
    /// that is neither terminal nor already cancelling, including queued and
    /// later (messaged or monitor-woken) runs.
    pub(crate) async fn live_session_runs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<RunId>, HaiderError> {
        let snapshot = self.child_session_snapshot(session_id).await?;
        let mut runs = snapshot
            .states
            .into_iter()
            .filter(|(_, state)| !state.is_terminal() && *state != RunState::Cancelling)
            .map(|(run_id, _)| run_id)
            .collect::<Vec<_>>();
        runs.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(runs)
    }

    async fn tree_roots(
        &self,
        root: &SubagentTreeRoot,
    ) -> Result<Vec<DelegationRecord>, HaiderError> {
        match root {
            SubagentTreeRoot::Run { session_id, run_id } => {
                self.hub
                    .delegations_for_parent_run(session_id.clone(), run_id.clone())
                    .await
            }
            SubagentTreeRoot::Session(session_id) => {
                self.hub
                    .delegations_for_parent_session(session_id.clone())
                    .await
            }
        }
    }

    /// Visits every descendant delegation below `root`, by session, exactly
    /// once. Unbounded depth; a revisited child session (a corrupt cycle)
    /// is skipped rather than looping.
    async fn walk_tree(
        &self,
        root: &SubagentTreeRoot,
    ) -> Result<Vec<DelegationRecord>, HaiderError> {
        let mut pending = self.tree_roots(root).await?;
        pending.reverse();
        let mut seen = HashSet::new();
        let (SubagentTreeRoot::Session(session_id) | SubagentTreeRoot::Run { session_id, .. }) =
            root;
        seen.insert(session_id.clone());
        let mut visited = Vec::new();
        while let Some(record) = pending.pop() {
            if !seen.insert(record.child_session_id.clone()) {
                continue;
            }
            let mut children = self
                .hub
                .delegations_for_parent_session(record.child_session_id.clone())
                .await?;
            children.reverse();
            pending.extend(children);
            visited.push(record);
        }
        Ok(visited)
    }

    /// Number of subagents in the tree with at least one live run.
    pub(crate) async fn live_subagent_count(
        &self,
        root: &SubagentTreeRoot,
    ) -> Result<u64, HaiderError> {
        let mut live = 0_u64;
        for record in self.walk_tree(root).await? {
            if !self
                .live_session_runs(&record.child_session_id)
                .await?
                .is_empty()
            {
                live = live.saturating_add(1);
            }
        }
        Ok(live)
    }

    /// Stop-all: cancels every live run of every descendant session and
    /// removes those sessions' monitors, repeating until one pass cancels
    /// nothing (closing races with concurrent spawns). The root session's
    /// own runs are never touched here.
    ///
    /// Every visited session is fenced for monitor registration and wake
    /// admission before its monitors are listed, and the fence is held until
    /// the cancelled runs settled and a final monitor drain ran (Astra
    /// 13-final B1): no run being stopped can leave a monitor behind that
    /// later restarts stopped work.
    #[cfg(test)]
    pub(crate) async fn stop_subagent_tree(
        &self,
        root: SubagentTreeRoot,
        command_prefix: &str,
    ) -> Result<SubagentStopSummary, HaiderError> {
        self.stop_subagent_tree_incarnations(root, command_prefix, None)
            .await
    }

    pub(crate) async fn stop_subagent_tree_incarnations(
        &self,
        root: SubagentTreeRoot,
        command_prefix: &str,
        incarnations: Option<&[StopIncarnation]>,
    ) -> Result<SubagentStopSummary, HaiderError> {
        let monitor = self.hub.inner_monitor();
        let fences = monitor.stop_fences().clone();
        let mut summary = SubagentStopSummary::default();
        let mut held = HashMap::<SessionId, MonitorStopFence>::new();
        let mut claimed = incarnations.map(<[StopIncarnation]>::to_vec);
        // Every run this request moved to cancelling, plus any run found
        // already cancelling in the tree: the stop converges all of them.
        let mut moved = Vec::<(SessionId, RunId)>::new();
        let mut targets = Vec::<(SessionId, RunId)>::new();
        loop {
            let mut cancelled_this_round = 0_u64;
            loop {
                let visited = self.walk_tree(&root).await?;
                summary.sessions = summary
                    .sessions
                    .max(u64::try_from(visited.len()).unwrap_or(u64::MAX));
                let mut cancelled_this_pass = 0_u64;
                for record in &visited {
                    if let Some(claimed) = &mut claimed
                        && !claimed
                            .iter()
                            .any(|target| target.session_id == record.child_session_id)
                    {
                        if !claimed.iter().any(|target| {
                            target.session_id == record.parent_session_id
                                && target.runs.contains(&record.parent_run_id)
                        }) {
                            continue;
                        }
                        // The delegation's original run is a stable child of
                        // an already claimed run. Do not capture later message
                        // runs that arrived before this walk discovered it.
                        claimed.push(StopIncarnation {
                            session_id: record.child_session_id.clone(),
                            runs: vec![record.child_run_id.clone()],
                            monitors: Vec::new(),
                        });
                    }
                    // Fence, then drain monitors, before any cancel: a
                    // monitor must not wake a session the same pass is about
                    // to stop, and no new one can be registered meanwhile.
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        held.entry(record.child_session_id.clone())
                    {
                        entry.insert(fences.fence(&record.child_session_id));
                        summary.monitors_removed = summary.monitors_removed.saturating_add(
                            monitor
                                .remove_for_stop_incarnation(
                                    &self.hub,
                                    &record.child_session_id,
                                    command_prefix,
                                    claimed.as_deref().and_then(|targets| {
                                        targets.iter().find(|target| {
                                            target.session_id == record.child_session_id
                                        })
                                    }),
                                )
                                .await?,
                        );
                        #[cfg(all(test, unix))]
                        first_drain_test_pause(&record.child_session_id, "tree").await;
                    }
                    let snapshot = self
                        .child_session_snapshot(&record.child_session_id)
                        .await?;
                    for (run_id, state) in &snapshot.states {
                        if *state == RunState::Cancelling
                            && stop_owns_run(claimed.as_deref(), &record.child_session_id, run_id)
                        {
                            let key = (record.child_session_id.clone(), run_id.clone());
                            if !targets.contains(&key) {
                                targets.push(key);
                            }
                        }
                    }
                    for run_id in self.live_session_runs(&record.child_session_id).await? {
                        if !stop_owns_run(claimed.as_deref(), &record.child_session_id, &run_id) {
                            continue;
                        }
                        let command = stop_cancel_command(
                            &self.hub,
                            record,
                            &run_id,
                            command_prefix,
                            "stop_all",
                        )?;
                        match self.hub.cancel_internal_turn(command).await {
                            Ok(cancelled)
                                if cancelled.status
                                    == haider_core::TurnCancellationStatus::Accepted =>
                            {
                                fences.mark_run_stopped(&record.child_session_id, &run_id);
                                cancelled_this_pass = cancelled_this_pass.saturating_add(1);
                                let key = (record.child_session_id.clone(), run_id.clone());
                                moved.push(key.clone());
                                if !targets.contains(&key) {
                                    targets.push(key);
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(
                                    agent = %record.agent_id,
                                    run = %run_id,
                                    ?error,
                                    "stop-all could not cancel a subagent run"
                                );
                            }
                        }
                    }
                }
                cancelled_this_round = cancelled_this_round.saturating_add(cancelled_this_pass);
                if cancelled_this_pass == 0 {
                    break;
                }
            }
            self.converge_cancelled_runs(&targets).await?;
            // Final drain after the targeted runs settled, still fenced.
            let mut still_live = false;
            for session in held.keys() {
                summary.monitors_removed = summary.monitors_removed.saturating_add(
                    monitor
                        .remove_for_stop_incarnation(
                            &self.hub,
                            session,
                            command_prefix,
                            claimed.as_deref().and_then(|targets| {
                                targets.iter().find(|target| &target.session_id == session)
                            }),
                        )
                        .await?,
                );
                still_live |= !self.live_session_runs(session).await?.is_empty();
            }
            // Repeat convergence only for the claimed incarnations. Later
            // messaged runs remain independent and are reported truthfully;
            // they must never become targets of this earlier intent.
            let late_descendant = if let Some(claimed) = &claimed {
                self.walk_tree(&root).await?.iter().any(|record| {
                    !claimed
                        .iter()
                        .any(|target| target.session_id == record.child_session_id)
                        && claimed.iter().any(|target| {
                            target.session_id == record.parent_session_id
                                && target.runs.contains(&record.parent_run_id)
                        })
                })
            } else {
                false
            };
            if !late_descendant && (!still_live || cancelled_this_round == 0) {
                break;
            }
        }
        // Report truthfully: count only runs this request moved that are
        // now durably Cancelled, and every subagent whose session still has
        // a non-terminal run (cancelling included).
        let mut confirmed = 0_u64;
        for (session_id, run_id) in &moved {
            if self.run_state_of(session_id, run_id).await? == Some(RunState::Cancelled) {
                confirmed = confirmed.saturating_add(1);
            }
        }
        summary.cancelled_runs = confirmed;
        summary.still_running = self.nonterminal_subagent_count(&root).await?;
        drop(held);
        Ok(summary)
    }

    /// Durable state of one run (journal authority).
    async fn run_state_of(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<Option<RunState>, HaiderError> {
        Ok(self
            .child_session_snapshot(session_id)
            .await?
            .states
            .get(run_id)
            .cloned())
    }

    /// Subagents in the tree with ANY non-terminal run, cancelling included.
    async fn nonterminal_subagent_count(
        &self,
        root: &SubagentTreeRoot,
    ) -> Result<u64, HaiderError> {
        let mut count = 0_u64;
        for record in self.walk_tree(root).await? {
            if self
                .child_session_snapshot(&record.child_session_id)
                .await?
                .states
                .values()
                .any(|state| !state.is_terminal())
            {
                count = count.saturating_add(1);
            }
        }
        Ok(count)
    }

    /// Drives cancelled runs to a terminal state. A run whose owning
    /// supervisor never started (an accepted-but-never-launched child) or
    /// exited without terminalizing is re-submitted to the worker manager,
    /// whose admission finalizes a durably cancelling run as Cancelled
    /// without provider work; a run already owned is a no-op there.
    async fn converge_cancelled_runs(
        &self,
        targets: &[(SessionId, RunId)],
    ) -> Result<(), HaiderError> {
        if targets.is_empty() {
            return Ok(());
        }
        // Runs already cancelling are stop targets too. Retain them if the
        // convergence bound expires before their session guard drops.
        let fences = self.hub.inner_monitor().stop_fences().clone();
        for (session_id, run_id) in targets {
            fences.mark_run_stopped(session_id, run_id);
        }
        let started = tokio::time::Instant::now();
        let mut resubmitted = HashSet::<RunId>::new();
        loop {
            let mut pending = Vec::new();
            for (session_id, run_id) in targets {
                if !self
                    .run_state_of(session_id, run_id)
                    .await?
                    .is_some_and(|state| state.is_terminal())
                {
                    pending.push((session_id, run_id));
                }
            }
            if pending.is_empty() {
                self.forget_stopped_runs(targets, &[]);
                return Ok(());
            }
            let elapsed = started.elapsed();
            if elapsed >= STOP_CONVERGENCE_RESUBMIT_AFTER {
                for (session_id, run_id) in &pending {
                    if resubmitted.insert((*run_id).clone())
                        && let Err(error) = self.resubmit_for_finalization(session_id, run_id).await
                    {
                        tracing::warn!(%run_id, ?error, "cancelled subagent run could not be re-submitted for finalization");
                    }
                }
            }
            if elapsed >= STOP_CONVERGENCE_BOUND {
                tracing::warn!(
                    pending = pending.len(),
                    "subagent stop did not observe every cancelled run terminal within its bound"
                );
                // A run still settling stays refused for monitor
                // registration after the stop's fence drops.
                self.forget_stopped_runs(targets, &pending);
                return Ok(());
            }
            tokio::time::sleep(STOP_CONVERGENCE_POLL).await;
        }
    }

    fn forget_stopped_runs(
        &self,
        targets: &[(SessionId, RunId)],
        pending: &[(&SessionId, &RunId)],
    ) {
        let fences = self.hub.inner_monitor().stop_fences().clone();
        for (session_id, run_id) in targets {
            if !pending
                .iter()
                .any(|(session, run)| *session == session_id && *run == run_id)
            {
                fences.forget_stopped_run(session_id, run_id);
            }
        }
    }

    /// Re-submits a durable run to the worker manager so its supervisor
    /// admission can finalize it (dedup-safe for an owned or queued run).
    async fn resubmit_for_finalization(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<(), HaiderError> {
        let mut cursor = 0;
        let mut accepted_at = None;
        loop {
            let page = self
                .hub
                .read_internal_session_for(
                    haider_platform::phase_trace::StoreReadCaller::Delegation,
                    session_id,
                    cursor,
                    256,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if envelope.run_id.as_ref() == Some(run_id)
                    && matches!(
                        envelope.payload.decode_event(),
                        Ok(haider_protocol::EventPayload::UserMessage { .. })
                    )
                {
                    accepted_at = Some((envelope.seq, envelope.branch_id.clone()));
                    break;
                }
            }
            if accepted_at.is_some() {
                break;
            }
        }
        let Some((accepted_seq, branch_id)) = accepted_at else {
            return Ok(());
        };
        let turn_ordinal = self
            .hub
            .turn_ordinal(session_id, run_id)
            .await?
            .unwrap_or(0);
        self.hub
            .submit_internal_turn(AcceptedTurn {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn_ordinal,
                accepted_seq,
                worker_generation: self.hub.worker_generation(),
                branch_id,
                disposition: haider_core::TurnAdmissionDisposition::Queued,
                first_user_turn: false,
                pdf_attachments: Vec::new(),
            })
            .await
    }

    /// True when the delegating run of `agent` is already cancelling or
    /// cancelled.
    pub(crate) async fn parent_run_stopping(&self, agent: &AgentId) -> Result<bool, HaiderError> {
        let Some(record) = self.hub.delegation(agent.clone()).await? else {
            return Ok(false);
        };
        Ok(matches!(
            self.run_state_of(&record.parent_session_id, &record.parent_run_id)
                .await?,
            Some(RunState::Cancelling | RunState::Cancelled)
        ))
    }

    /// Moves a never-launched child's accepted turn to cancelling; the
    /// following submission finalizes it as Cancelled.
    pub(crate) async fn cancel_unlaunched_child(&self, agent: &AgentId) -> Result<(), HaiderError> {
        let Some(record) = self.hub.delegation(agent.clone()).await? else {
            return Ok(());
        };
        let command = stop_cancel_command(
            &self.hub,
            &record,
            &record.child_run_id,
            &format!("parent-stopped-{}", record.agent_id),
            "parent_stopped",
        )?;
        self.hub.cancel_internal_turn(command).await.map(|_| ())
    }

    /// Stops ONE named subagent: its original run (the receipt-bearing
    /// `primary` command supplied by the caller), every later live run, and
    /// its own monitors. Its children are deliberately left running.
    ///
    /// The caller validated the request's generation and command identity
    /// before calling (Astra 13-final B4). The session is fenced for monitor
    /// registration and wake admission first; the primary cancel is the
    /// first durable effect, so a refused primary leaves nothing changed.
    /// Monitors are removed only after it, and a final drain runs once the
    /// cancelled runs settled (B1).
    #[cfg(all(test, unix))]
    pub(crate) async fn cancel_single_subagent(
        &self,
        record: &DelegationRecord,
        primary: TurnCancelCommand,
        command_prefix: &str,
    ) -> Result<haider_core::CancelledTurn, HaiderError> {
        self.cancel_single_subagent_incarnation(record, primary, command_prefix, None)
            .await
    }

    pub(crate) async fn cancel_single_subagent_incarnation(
        &self,
        record: &DelegationRecord,
        primary: TurnCancelCommand,
        command_prefix: &str,
        incarnation: Option<StopIncarnation>,
    ) -> Result<haider_core::CancelledTurn, HaiderError> {
        let monitor = self.hub.inner_monitor();
        let fences = monitor.stop_fences().clone();
        let fence = fences.fence(&record.child_session_id);
        let primary_run = primary.run_id.clone();
        let mut primary_result = self.hub.cancel_internal_turn(primary).await?;
        if primary_result.status == haider_core::TurnCancellationStatus::Accepted {
            fences.mark_run_stopped(&record.child_session_id, &primary_run);
        }
        let monitors_removed = monitor
            .remove_for_stop_incarnation(
                &self.hub,
                &record.child_session_id,
                command_prefix,
                incarnation.as_ref(),
            )
            .await?;
        #[cfg(all(test, unix))]
        first_drain_test_pause(&record.child_session_id, "named").await;
        for cancelled in self
            .cancel_live_runs(record, command_prefix, incarnation.as_ref())
            .await?
        {
            // The original run finished earlier; report the later run this
            // request actually stopped.
            if primary_result.status == haider_core::TurnCancellationStatus::AlreadyTerminal {
                primary_result = cancelled;
            }
        }
        tracing::debug!(
            agent = %record.agent_id,
            monitors_removed,
            "named subagent cancelled without its children"
        );
        // Acceptance is the response. Convergence (finalizing a run whose
        // supervisor never started) and the final monitor drain continue in
        // the background while the fence is held.
        let coordinator = self.clone();
        let record = record.clone();
        let command_prefix = command_prefix.to_owned();
        tokio::spawn(async move {
            if let Err(error) = coordinator
                .settle_named_cancel(&record, &command_prefix, incarnation.as_ref())
                .await
            {
                tracing::warn!(?error, "named subagent cancellation did not converge");
            }
            drop(fence);
        });
        Ok(primary_result)
    }

    /// Cancels every live run of one subagent session until a pass finds
    /// none; returns the accepted cancellations.
    async fn cancel_live_runs(
        &self,
        record: &DelegationRecord,
        command_prefix: &str,
        incarnation: Option<&StopIncarnation>,
    ) -> Result<Vec<haider_core::CancelledTurn>, HaiderError> {
        let fences = self.hub.inner_monitor().stop_fences().clone();
        let mut accepted = Vec::new();
        loop {
            let mut this_pass = 0_usize;
            for run_id in self.live_session_runs(&record.child_session_id).await? {
                if incarnation.is_some_and(|target| !target.runs.contains(&run_id)) {
                    continue;
                }
                let command =
                    stop_cancel_command(&self.hub, record, &run_id, command_prefix, "manual")?;
                let cancelled = self.hub.cancel_internal_turn(command).await?;
                if cancelled.status == haider_core::TurnCancellationStatus::Accepted {
                    fences.mark_run_stopped(&record.child_session_id, &run_id);
                    this_pass = this_pass.saturating_add(1);
                    accepted.push(cancelled);
                }
            }
            if this_pass == 0 {
                return Ok(accepted);
            }
        }
    }

    /// Background tail of a named cancel: converge the cancelled runs, then
    /// drain monitors again and cancel any run admitted meanwhile.
    async fn settle_named_cancel(
        &self,
        record: &DelegationRecord,
        command_prefix: &str,
        incarnation: Option<&StopIncarnation>,
    ) -> Result<(), HaiderError> {
        loop {
            let targets = self
                .child_session_snapshot(&record.child_session_id)
                .await?
                .states
                .into_iter()
                .filter(|(run, state)| {
                    *state == RunState::Cancelling
                        && incarnation.is_none_or(|target| target.runs.contains(run))
                })
                .map(|(run_id, _)| (record.child_session_id.clone(), run_id))
                .collect::<Vec<_>>();
            self.converge_cancelled_runs(&targets).await?;
            self.hub
                .inner_monitor()
                .remove_for_stop_incarnation(
                    &self.hub,
                    &record.child_session_id,
                    command_prefix,
                    incarnation,
                )
                .await?;
            if self
                .cancel_live_runs(record, command_prefix, incarnation)
                .await?
                .is_empty()
            {
                return Ok(());
            }
        }
    }

    /// Main-only stop of `parent_run_id`: never cancels a child. Records a
    /// durable delivery obligation and arms result delivery for every
    /// foreground child whose result the parent has not collected, and
    /// journals the display marker when subagents live on.
    pub(crate) async fn detach_parent_run_children(
        &self,
        parent_session_id: &SessionId,
        parent_run_id: &RunId,
    ) -> Result<u64, HaiderError> {
        let direct = self
            .hub
            .delegations_for_parent_run(parent_session_id.clone(), parent_run_id.clone())
            .await?;
        let Some(first) = direct.first().cloned() else {
            return Ok(0);
        };
        let collected = self.parent_child_results(parent_session_id).await?;
        let mut agents = Vec::new();
        let mut owed = Vec::new();
        for record in &direct {
            let live = !self
                .live_session_runs(&record.child_session_id)
                .await?
                .is_empty();
            // Owed = a foreground child whose terminal result the parent has
            // not recorded, whatever the child's state: a stop-all may have
            // cancelled it, or it may already be terminal (Astra 13-final
            // B3). The durable obligation never depends on the display list.
            let deliver = !is_background_record(record)
                && record.state != DelegationState::Collected
                && !collected.contains(&record.agent_id);
            if deliver {
                owed.push(record.clone());
            }
            if live || deliver {
                agents.push(StillRunningAgentV1 {
                    agent: record.agent_id.clone(),
                    task: record.task.clone(),
                    result_pending: deliver,
                });
            }
        }
        self.record_owed_deliveries(&owed, DetachedReasonV1::MainStopped)
            .await?;
        for record in owed {
            self.arm_detached_delivery(record, DetachedReasonV1::MainStopped);
        }
        let still_running = self
            .live_subagent_count(&SubagentTreeRoot::Run {
                session_id: parent_session_id.clone(),
                run_id: parent_run_id.clone(),
            })
            .await?;
        if still_running == 0 {
            return Ok(0);
        }
        let event_id = format!(
            "subagents-still-running-{}",
            &stable_digest(&[parent_session_id.as_str(), parent_run_id.as_str()])[..24]
        );
        if self.session_has_event(parent_session_id, &event_id).await? {
            return Ok(still_running);
        }
        // Owed results first in the bounded display list.
        agents.sort_by_key(|agent| !agent.result_pending);
        let agents_truncated = agents.len() > STILL_RUNNING_AGENT_LIST_MAX;
        agents.truncate(STILL_RUNNING_AGENT_LIST_MAX);
        let marker = SubagentsStillRunningV1 {
            parent_session_id: parent_session_id.clone(),
            stopped_run_id: parent_run_id.clone(),
            still_running,
            agents,
            agents_truncated,
        };
        let item = marker.to_extension_item().map_err(internal_serialization)?;
        let mut envelope = [parent_fact_envelope(
            &first,
            &event_id,
            haider_protocol::EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(event_id.clone()),
                item,
            }),
            PromptRender::Verbatim,
            self.hub.device_id(),
            self.hub.worker_generation(),
        )?];
        if let Err(error) = self.hub.append(&mut envelope).await {
            // A concurrent detach of the same run may have committed the
            // identical marker first (event ids are unique).
            if !self.session_has_event(parent_session_id, &event_id).await? {
                return Err(error);
            }
        }
        Ok(still_running)
    }

    /// A blocking spawn's explicit `wait_ms` observation ended and the
    /// parent turn went on without the child (Astra 13-final B2). Called
    /// once the foreground collector has committed its `child_still_running`
    /// result and released the child, so it never races an active
    /// collector: the child's terminal result is delivered to the parent
    /// session later and becomes collectible by `agent wait`.
    pub(crate) async fn detach_observed_child(&self, agent: &AgentId) -> Result<(), HaiderError> {
        let Some(record) = self.hub.delegation(agent.clone()).await? else {
            return Ok(());
        };
        if is_background_record(&record) || record.state == DelegationState::Collected {
            return Ok(());
        }
        self.record_owed_deliveries(
            std::slice::from_ref(&record),
            DetachedReasonV1::ObservationEnded,
        )
        .await?;
        self.arm_detached_delivery(record, DetachedReasonV1::ObservationEnded);
        Ok(())
    }

    /// Journals one durable `owed` fact per child (idempotent by event id,
    /// never truncated), before any in-process watcher is armed.
    async fn record_owed_deliveries(
        &self,
        owed: &[DelegationRecord],
        reason: DetachedReasonV1,
    ) -> Result<(), HaiderError> {
        let Some(first) = owed.first() else {
            return Ok(());
        };
        let existing = self
            .session_event_ids_with_prefix(&first.parent_session_id, "detached-delivery-owed-")
            .await?;
        let mut envelopes = Vec::new();
        for record in owed {
            let event_id = delivery_fact_id("owed", record);
            if existing.contains(&event_id) {
                continue;
            }
            envelopes.push(delivery_fact_envelope(
                self,
                record,
                &event_id,
                DetachedDeliveryPhaseV1::Owed,
                reason,
            )?);
        }
        if envelopes.is_empty() {
            return Ok(());
        }
        if let Err(error) = self.hub.append(&mut envelopes).await {
            // A concurrent detach may have committed some of them first;
            // fall back to one append per missing fact.
            let existing = self
                .session_event_ids_with_prefix(&first.parent_session_id, "detached-delivery-owed-")
                .await?;
            for envelope in envelopes {
                if existing.contains(envelope.event_id.as_str()) {
                    continue;
                }
                let event_id = envelope.event_id.clone();
                if let Err(retry) = self.hub.append(&mut [envelope]).await
                    && !self
                        .session_has_event(&first.parent_session_id, event_id.as_str())
                        .await?
                {
                    tracing::warn!(?error, "owed detached delivery batch was refused");
                    return Err(retry);
                }
            }
        }
        Ok(())
    }

    /// Startup recovery: re-arms result delivery for every child a durable
    /// obligation still owes a result, WHATEVER its state. A child that was
    /// cancelled or failed while its delivery was owed (for example by a
    /// graceful daemon stop) still delivers that cancelled/errored result,
    /// so a foreground `agent wait` on it resolves (9-verify3 B-2).
    pub(crate) async fn rearm_detached_deliveries(
        &self,
        agents: &[(AgentId, DetachedReasonV1)],
    ) -> Result<(), HaiderError> {
        for (agent, reason) in agents {
            if let Some(record) = self.hub.delegation(agent.clone()).await? {
                self.arm_detached_delivery(record, *reason);
            }
        }
        Ok(())
    }

    /// True while any in-process delivery watcher still owes a result.
    pub(crate) fn detached_deliveries_pending() -> bool {
        !DETACHED_WATCHERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn arm_detached_delivery(&self, record: DelegationRecord, reason: DetachedReasonV1) {
        {
            let mut watchers = DETACHED_WATCHERS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !watchers.insert(record.agent_id.clone()) {
                return;
            }
        }
        let coordinator = self.clone();
        tokio::spawn(async move {
            let agent = record.agent_id.clone();
            if let Err(error) = coordinator.deliver_detached_result(record, reason).await {
                tracing::warn!(%agent, ?error, "detached subagent result was not delivered");
            }
            DETACHED_WATCHERS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&agent);
        });
    }

    /// Agents whose `ChildResult` the parent session already recorded.
    async fn parent_child_results(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<HashSet<AgentId>, HaiderError> {
        let mut cursor = 0;
        let mut agents = HashSet::new();
        loop {
            let page = self
                .hub
                .read_internal_session_for(
                    haider_platform::phase_trace::StoreReadCaller::Delegation,
                    parent_session_id,
                    cursor,
                    256,
                )
                .await?;
            if page.is_empty() {
                return Ok(agents);
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if let Ok(haider_protocol::EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ChildResult { report },
                    ..
                })) = envelope.payload.decode_event()
                {
                    agents.insert(report.agent);
                }
            }
        }
    }

    /// Ends an obligation that needs no detached delivery.
    async fn settle_owed_delivery(
        &self,
        record: &DelegationRecord,
        reason: DetachedReasonV1,
    ) -> Result<(), HaiderError> {
        let event_id = delivery_fact_id("settled", record);
        if self
            .session_has_event(&record.parent_session_id, &event_id)
            .await?
        {
            return Ok(());
        }
        let mut envelope = [delivery_fact_envelope(
            self,
            record,
            &event_id,
            DetachedDeliveryPhaseV1::Settled,
            reason,
        )?];
        if let Err(error) = self.hub.append(&mut envelope).await
            && !self
                .session_has_event(&record.parent_session_id, &event_id)
                .await?
        {
            return Err(error);
        }
        Ok(())
    }

    async fn deliver_detached_result(
        &self,
        record: DelegationRecord,
        reason: DetachedReasonV1,
    ) -> Result<(), HaiderError> {
        let event_id = format!(
            "detached-child-result-{}",
            &stable_digest(&[record.parent_session_id.as_str(), record.agent_id.as_str()])[..24]
        );
        if self
            .session_has_event(&record.parent_session_id, &event_id)
            .await?
        {
            // Already delivered; close any obligation journalled after it
            // (a late owed fact from a raced detach) so recovery stops
            // re-arming it.
            return self.settle_owed_delivery(&record, reason).await;
        }
        let ticket = DeferredTicket {
            id: record.agent_id.as_str().to_owned(),
            manifest: record.manifest.clone(),
        };
        let mut mirror = self.load_chip_mirror(&ticket).await?;
        let current = loop {
            let Some(current) = self.hub.delegation(record.agent_id.clone()).await? else {
                return self.settle_owed_delivery(&record, reason).await;
            };
            self.mirror_child_chip_states(&current, &mut mirror).await?;
            if mirror.child_run_terminal {
                break current;
            }
            tokio::time::sleep(DETACHED_DELIVERY_POLL).await;
        };
        // The same bounded terminal tail a collector runs: flush the child's
        // final chip and metrics projection into the parent.
        let settlement = self
            .mirror_until_child_terminal(
                &current,
                &mut mirror,
                tokio::time::Instant::now() + self.settlement_tail_timeout,
            )
            .await?;
        report_child_settlement_outcome(settlement);
        // Terminal/collection race: a foreground collector may have recorded
        // the child's result meanwhile. Never deliver a second copy; end the
        // durable obligation instead.
        let current = self
            .hub
            .delegation(current.agent_id.clone())
            .await?
            .unwrap_or(current);
        if current.state == DelegationState::Collected
            || self
                .parent_child_results(&current.parent_session_id)
                .await?
                .contains(&current.agent_id)
        {
            return self.settle_owed_delivery(&current, reason).await;
        }
        let (report, chip) = match current.report.clone() {
            Some(report) => {
                let chip = if report.verified == ReportVerification::Red {
                    ChipState::Error
                } else {
                    ChipState::Done
                };
                (report, chip)
            }
            None => {
                let Some(completion) = self.derive_terminal_report(&current).await? else {
                    return self.settle_owed_delivery(&current, reason).await;
                };
                let stored = self
                    .hub
                    .record_delegation_report(current.agent_id.clone(), completion.report.clone())
                    .await?;
                (stored.report.unwrap_or(completion.report), completion.chip)
            }
        };
        let state = match self.session_progress(&current).await?.state {
            Some(RunState::Done) => DetachedChildStateV1::Done,
            Some(RunState::Cancelled) => DetachedChildStateV1::Cancelled,
            _ => DetachedChildStateV1::Errored,
        };
        let (summary, summary_truncated) =
            bounded_summary(&report.summary, DETACHED_RESULT_SUMMARY_MAX_BYTES);
        let delivered = DetachedChildResultV1 {
            agent: current.agent_id.clone(),
            task: current.task.clone(),
            stopped_run_id: current.parent_run_id.clone(),
            state,
            verified: report.verified.clone(),
            summary,
            summary_truncated,
            reason,
        };
        let steered = self.steer_detached_result(&current, &delivered).await;
        let item = delivered
            .to_extension_item()
            .map_err(internal_serialization)?;
        let device = self.hub.device_id();
        let generation = self.hub.worker_generation();
        let result_item = format!("{event_id}-child-result");
        let mut envelopes = [
            parent_fact_envelope(
                &current,
                &format!("{event_id}-report"),
                haider_protocol::EventPayload::AgentReport(report.clone()),
                PromptRender::Omit,
                device.clone(),
                generation,
            )?,
            parent_fact_envelope(
                &current,
                &format!("{event_id}-chip"),
                haider_protocol::EventPayload::AgentChipState {
                    agent: current.agent_id.clone(),
                    chip,
                },
                PromptRender::Omit,
                device.clone(),
                generation,
            )?,
            parent_fact_envelope(
                &current,
                &format!("{result_item}-started"),
                haider_protocol::EventPayload::Item(ItemEvent::Started {
                    item_id: ItemId::new(result_item.clone()),
                    item: TurnItem::ChildResult {
                        report: report.clone(),
                    },
                }),
                PromptRender::Omit,
                device.clone(),
                generation,
            )?,
            parent_fact_envelope(
                &current,
                &format!("{result_item}-completed"),
                haider_protocol::EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new(result_item),
                    item: TurnItem::ChildResult { report },
                }),
                PromptRender::Omit,
                device.clone(),
                generation,
            )?,
            // Exactly one prompt copy: the durable steer owns it when the
            // result reached an active run; otherwise this fact does.
            parent_fact_envelope(
                &current,
                &event_id,
                haider_protocol::EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new(event_id.clone()),
                    item,
                }),
                if steered {
                    PromptRender::Omit
                } else {
                    PromptRender::Verbatim
                },
                device,
                generation,
            )?,
        ];
        self.hub.append(&mut envelopes).await?;
        if current.state != DelegationState::Collected {
            self.hub
                .mark_delegation_collected(current.agent_id.clone())
                .await?;
        }
        Ok(())
    }

    /// Durable steer into the parent's active run (the task-completion S1
    /// seam). Returns false when no run can take it; a stray admission from
    /// a lost race is cancelled so delivery never starts provider work.
    async fn steer_detached_result(
        &self,
        record: &DelegationRecord,
        delivered: &DetachedChildResultV1,
    ) -> bool {
        let active = match self.active_parent_run(&record.parent_session_id).await {
            Ok(Some(active)) => active,
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!(agent = %record.agent_id, ?error, "active parent run scan failed");
                return false;
            }
        };
        let (run_id, branch_id) = active;
        let text = delivered.model_note();
        let identity = stable_digest(&[record.agent_id.as_str(), run_id.as_str()]);
        let identity = &identity[..24];
        let Ok(request_json) = serde_json::to_string(&serde_json::json!({
            "session_id": record.parent_session_id,
            "run_id": run_id,
            "agent": record.agent_id,
            "text": text,
            "mode": DeliveryMode::Steer,
            "delivery_path": "detached_child_result",
        })) else {
            return false;
        };
        let accepted = match self
            .hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("detached-result-steer-{identity}"),
                request_digest: digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: record.parent_session_id.clone(),
                worker_generation: self.hub.worker_generation(),
                branch_id,
                run_id,
                agent_id: record.parent_agent_id.clone(),
                text: text.clone(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
                queued_event_id: EventId::new(format!("detached-result-queued-{identity}")),
                user_event_id: EventId::new(format!("detached-result-user-{identity}")),
                active_event_id: EventId::new(format!("detached-result-active-{identity}")),
                device_id: self.hub.device_id(),
            })
            .await
        {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(agent = %record.agent_id, ?error, "detached result steer was not accepted");
                return false;
            }
        };
        if accepted.disposition != haider_core::TurnAdmissionDisposition::SteerPending {
            let Ok(request_json) = serde_json::to_string(&serde_json::json!({
                "session_id": record.parent_session_id,
                "run_id": accepted.run_id,
                "reason": "detached-result-steer-race",
            })) else {
                return false;
            };
            let cancel = TurnCancelCommand {
                command_id: format!("detached-result-race-cancel-{identity}"),
                request_digest: digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: record.parent_session_id.clone(),
                worker_generation: self.hub.worker_generation(),
                run_id: accepted.run_id.clone(),
                cancelling_event_id: EventId::new(format!(
                    "detached-result-race-cancelling-{identity}"
                )),
                device_id: self.hub.device_id(),
            };
            if let Err(error) = self.hub.cancel_internal_turn(cancel).await {
                tracing::warn!(agent = %record.agent_id, ?error, "stray detached-result admission was not cancelled");
            }
            return false;
        }
        if let Err(error) = self.hub.submit_internal_nudge(accepted, text).await {
            tracing::warn!(agent = %record.agent_id, ?error, "detached result steer wake was not delivered");
        }
        true
    }

    /// Latest parent run a steer can reach: nonterminal, not cancelling and
    /// not merely queued.
    async fn active_parent_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(RunId, Option<BranchId>)>, HaiderError> {
        let mut cursor = 0;
        let mut states = HashMap::<RunId, (RunState, Option<BranchId>, u64)>::new();
        loop {
            let page = self
                .hub
                .read_internal_session_for(
                    haider_platform::phase_trace::StoreReadCaller::Delegation,
                    session_id,
                    cursor,
                    256,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                let Some(run_id) = envelope.run_id.clone() else {
                    continue;
                };
                if let Ok(haider_protocol::EventPayload::RunState(state)) =
                    envelope.payload.decode_event()
                {
                    states.insert(run_id, (state, envelope.branch_id, envelope.seq));
                }
            }
        }
        Ok(states
            .into_iter()
            .filter(|(_, (state, _, _))| {
                !state.is_terminal() && *state != RunState::Cancelling && *state != RunState::Queued
            })
            .max_by_key(|(_, (_, _, seq))| *seq)
            .map(|(run_id, (_, branch_id, _))| (run_id, branch_id)))
    }

    async fn session_event_ids_with_prefix(
        &self,
        session_id: &SessionId,
        prefix: &str,
    ) -> Result<HashSet<String>, HaiderError> {
        let mut cursor = 0;
        let mut found = HashSet::new();
        loop {
            let page = self
                .hub
                .read_internal_session_for(
                    haider_platform::phase_trace::StoreReadCaller::Delegation,
                    session_id,
                    cursor,
                    256,
                )
                .await?;
            if page.is_empty() {
                return Ok(found);
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            found.extend(
                page.iter()
                    .filter(|envelope| envelope.event_id.as_str().starts_with(prefix))
                    .map(|envelope| envelope.event_id.as_str().to_owned()),
            );
        }
    }

    async fn session_has_event(
        &self,
        session_id: &SessionId,
        event_id: &str,
    ) -> Result<bool, HaiderError> {
        let mut cursor = 0;
        loop {
            let page = self
                .hub
                .read_internal_session_for(
                    haider_platform::phase_trace::StoreReadCaller::Delegation,
                    session_id,
                    cursor,
                    256,
                )
                .await?;
            if page.is_empty() {
                return Ok(false);
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            if page
                .iter()
                .any(|envelope| envelope.event_id.as_str() == event_id)
            {
                return Ok(true);
            }
        }
    }
}

/// Receipt-backed cancel of one subagent run under a stop request.
fn stop_cancel_command(
    hub: &SessionHub,
    record: &DelegationRecord,
    run_id: &RunId,
    command_prefix: &str,
    reason: &str,
) -> Result<TurnCancelCommand, HaiderError> {
    let request_json = serde_json::to_string(&serde_json::json!({
        "session_id": record.child_session_id,
        "run_id": run_id,
        "agent": record.agent_id,
        "reason": reason,
    }))
    .map_err(internal_serialization)?;
    let identity = stable_digest(&[
        command_prefix,
        record.child_session_id.as_str(),
        run_id.as_str(),
    ]);
    let identity = &identity[..32];
    Ok(TurnCancelCommand {
        command_id: format!("{command_prefix}-{reason}-{identity}"),
        request_digest: digest_bytes(request_json.as_bytes()),
        request_json,
        session_id: record.child_session_id.clone(),
        worker_generation: hub.worker_generation(),
        run_id: run_id.clone(),
        cancelling_event_id: EventId::new(format!("subagent-stop-cancelling-{identity}")),
        device_id: hub.device_id(),
    })
}

fn delivery_fact_id(phase: &str, record: &DelegationRecord) -> String {
    format!(
        "detached-delivery-{phase}-{}",
        &stable_digest(&[record.parent_session_id.as_str(), record.agent_id.as_str()])[..24]
    )
}

/// A durable, display-free delivery obligation fact in the parent session,
/// under the delegating run so recovery reduces it with that run.
fn delivery_fact_envelope(
    coordinator: &DelegationHandle,
    record: &DelegationRecord,
    event_id: &str,
    phase: DetachedDeliveryPhaseV1,
    reason: DetachedReasonV1,
) -> Result<RawEnvelope, HaiderError> {
    let item = DetachedDeliveryV1 {
        agent: record.agent_id.clone(),
        phase,
        reason,
    }
    .to_extension_item()
    .map_err(internal_serialization)?;
    let mut envelope = parent_fact_envelope(
        record,
        event_id,
        haider_protocol::EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(event_id),
            item,
        }),
        PromptRender::Omit,
        coordinator.hub.device_id(),
        coordinator.hub.worker_generation(),
    )?;
    envelope.render.ui = false;
    Ok(envelope)
}

/// A parent-session fact scoped to the delegating run's branch and agent.
fn parent_fact_envelope(
    record: &DelegationRecord,
    event_id: &str,
    payload: haider_protocol::EventPayload,
    prompt: PromptRender,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
) -> Result<RawEnvelope, HaiderError> {
    let payload = serde_json::to_value(payload).map_err(internal_serialization)?;
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: record.parent_session_id.clone(),
        branch_id: record.parent_branch_id.clone(),
        run_id: Some(record.parent_run_id.clone()),
        agent_id: record.parent_agent_id.clone(),
        device_id,
        authority_epoch: 0,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt,
        },
        payload: payload.into(),
    })
}

fn stop_owns_run(
    incarnations: Option<&[StopIncarnation]>,
    session: &SessionId,
    run: &RunId,
) -> bool {
    incarnations.is_none_or(|targets| {
        targets
            .iter()
            .any(|target| &target.session_id == session && target.runs.contains(run))
    })
}
