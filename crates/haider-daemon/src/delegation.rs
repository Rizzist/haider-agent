//! Daemon-owned local delegation coordinator.
//!
//! This is the only cross-session authority reachable from a parent tool.
//! It exposes typed spawn/collect operations, never a raw store or child
//! session address.

use crate::session_hub::SessionHub;
use haider_core::{
    AcceptedTurn, CancelToken, DeferredTicket, DeferredToolResult, DelegationCreateOutcome,
    DelegationRecord, DelegationState, SessionCreateCommand, TurnAcceptCommand, TurnCancelCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::agent::{
    AgentManifest, AgentRole, ChildReport, ChipState, Grant, Placement, ReportVerification,
};
use haider_protocol::effect::EffectClass;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{AgentId, EventId, ItemId, LeaseId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::RunState;
use haider_tools::SpawnSubagent;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CHILD_STALL_DEADLINE: Duration = Duration::from_secs(120);
pub(crate) const RECURSION_DEPTH_LIMIT: u32 = 3;
pub(crate) const RECURSION_LIMIT_MESSAGE: &str = "recursion depth limit";
const STALL_NUDGE_TEXT: &str = "report your status or conclude";
const STALL_REPORT_SUMMARY: &str =
    "subagent stalled after one nudge and was cancelled without further progress";
const MAX_REPORT_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct DelegationHandle {
    hub: SessionHub,
    stall_deadline: Duration,
}

pub(crate) struct SpawnCoordinates {
    pub(crate) parent_session_id: SessionId,
    pub(crate) parent_run_id: RunId,
    pub(crate) parent_agent_id: Option<AgentId>,
    pub(crate) tool_item_id: ItemId,
    pub(crate) call_id: String,
    pub(crate) metadata: SessionMetadataV1,
}

pub(crate) struct EstablishedSpawn {
    pub(crate) ticket: DeferredTicket,
    accepted: AcceptedTurn,
}

impl DelegationHandle {
    pub(crate) fn new(hub: SessionHub) -> Self {
        Self {
            hub,
            stall_deadline: CHILD_STALL_DEADLINE,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_stall_deadline(hub: SessionHub, stall_deadline: Duration) -> Self {
        Self {
            hub,
            stall_deadline,
        }
    }

    /// Establishes child session, durable link, and accepted first turn. It
    /// deliberately does not submit worker work: the broker must journal the
    /// `AgentSpawn` outcome before [`Self::launch`] crosses that boundary.
    pub(crate) async fn establish(
        &self,
        coordinates: SpawnCoordinates,
        request: SpawnSubagent,
    ) -> Result<EstablishedSpawn, HaiderError> {
        let ancestry = self
            .spawn_ancestry(
                &coordinates.parent_session_id,
                coordinates.parent_agent_id.as_ref(),
            )
            .await?;
        let identity = stable_digest(&[
            coordinates.parent_session_id.as_str(),
            coordinates.parent_run_id.as_str(),
            &coordinates.call_id,
        ]);
        let agent_id = AgentId::new(format!("agent-{identity}"));
        let child_session_id = SessionId::new(format!("session-child-{identity}"));
        let child_run_id = RunId::new(format!("run-child-{identity}"));
        let lease = LeaseId::new(format!("lease-child-{identity}"));
        let callsign_suffix = identity.get(..8).unwrap_or(identity.as_str());
        let manifest = AgentManifest {
            agent: agent_id.clone(),
            role: AgentRole::Subagent,
            task: request.task.clone(),
            callsign: Some(format!("SUB-{callsign_suffix}")),
            model_profile: coordinates.metadata.model.clone(),
            grant: Grant {
                tools: vec![
                    "fs_read".into(),
                    "fs_list".into(),
                    "fs_search".into(),
                    "fs_write".into(),
                    "fs_patch".into(),
                    "process_exec".into(),
                    "spawn_subagent".into(),
                ],
                effect_ceiling: vec![
                    EffectClass::FsRead,
                    EffectClass::FsWrite,
                    EffectClass::ProcessExec,
                    EffectClass::AgentSpawn,
                ],
            },
            budget_tokens: Some(coordinates.metadata.max_tokens),
            placement: Placement::Local,
            lease,
            fencing_epoch: self.hub.worker_generation(),
            attempt: 0,
            parent: coordinates.parent_agent_id.clone(),
            coordinates: Some(serde_json::json!({
                "parent_session_id": coordinates.parent_session_id,
                "parent_run_id": coordinates.parent_run_id,
                "call_id": coordinates.call_id,
                "tool_item_id": coordinates.tool_item_id,
            })),
        };
        let create_json = serde_json::to_string(&serde_json::json!({
            "cwd": coordinates.metadata.cwd,
            "provider": coordinates.metadata.provider,
            "model": coordinates.metadata.model,
            "max_tokens": coordinates.metadata.max_tokens,
            "delegation_agent": agent_id,
        }))
        .map_err(internal_serialization)?;
        let create_digest = digest_bytes(create_json.as_bytes());
        self.hub
            .create_internal_session(SessionCreateCommand {
                command_id: format!("delegation-session-{identity}"),
                request_digest: create_digest,
                request_json: create_json,
                session_id: child_session_id.clone(),
                cwd: coordinates.metadata.cwd.clone(),
                provider: coordinates.metadata.provider.clone(),
                model: coordinates.metadata.model.clone(),
                max_tokens: coordinates.metadata.max_tokens,
                system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
                event_id: EventId::new(format!("delegation-created-{identity}")),
                device_id: self.hub.device_id(),
            })
            .await?;

        let record = DelegationRecord {
            agent_id: agent_id.clone(),
            child_session_id: child_session_id.clone(),
            child_run_id: child_run_id.clone(),
            parent_session_id: coordinates.parent_session_id,
            parent_run_id: coordinates.parent_run_id,
            call_id: coordinates.call_id,
            tool_item_id: coordinates.tool_item_id,
            parent_agent_id: coordinates.parent_agent_id,
            root_session_id: ancestry.root_session_id,
            depth: ancestry.depth,
            task: request.task.clone(),
            prompt: request.prompt.clone(),
            manifest: manifest.clone(),
            state: DelegationState::Spawned,
            report: None,
        };
        let record = match self.hub.create_delegation(record).await? {
            DelegationCreateOutcome::Committed(record)
            | DelegationCreateOutcome::IdempotentReplay(record) => record,
        };
        let turn_text = format!(
            "Delegated task: {}\n\n{}\n\nReturn a concise final report for the parent agent.",
            record.task, record.prompt
        );
        let turn_json = serde_json::to_string(&serde_json::json!({
            "session_id": record.child_session_id,
            "worker_generation": self.hub.worker_generation(),
            "text": turn_text,
            "attachments": [],
            "mode": DeliveryMode::Steer,
            "delegation_agent": record.agent_id,
        }))
        .map_err(internal_serialization)?;
        let accepted = self
            .hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("delegation-turn-{identity}"),
                request_digest: digest_bytes(turn_json.as_bytes()),
                request_json: turn_json,
                session_id: record.child_session_id,
                worker_generation: self.hub.worker_generation(),
                run_id: record.child_run_id,
                agent_id: Some(record.agent_id.clone()),
                text: turn_text,
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
                queued_event_id: EventId::new(format!("delegation-queued-{identity}")),
                user_event_id: EventId::new(format!("delegation-user-{identity}")),
                active_event_id: EventId::new(format!("delegation-active-{identity}")),
                device_id: self.hub.device_id(),
            })
            .await?;
        Ok(EstablishedSpawn {
            ticket: DeferredTicket {
                id: agent_id.as_str().to_owned(),
                manifest,
            },
            accepted,
        })
    }

    pub(crate) async fn validate_spawn_depth(
        &self,
        parent_session_id: &SessionId,
        parent_agent_id: Option<&AgentId>,
    ) -> Result<(), HaiderError> {
        self.spawn_ancestry(parent_session_id, parent_agent_id)
            .await
            .map(|_| ())
    }

    pub(crate) async fn launch(&self, established: &EstablishedSpawn) -> Result<(), HaiderError> {
        self.hub
            .mark_delegation_running(established.ticket.manifest.agent.clone())
            .await?;
        self.hub
            .submit_internal_turn(established.accepted.clone())
            .await
    }

    pub(crate) async fn record_launch_failure(
        &self,
        ticket: &DeferredTicket,
        error: &HaiderError,
    ) -> Result<(), HaiderError> {
        let report = ChildReport {
            agent: ticket.manifest.agent.clone(),
            summary: haider_core::sanitized_failure_message(&error.message),
            verified: ReportVerification::Red,
            workspace_revision: None,
        };
        self.hub
            .record_delegation_report(ticket.manifest.agent.clone(), report)
            .await
            .map(|_| ())
    }

    pub(crate) async fn collect(
        &self,
        ticket: &DeferredTicket,
        cancel: &CancelToken,
    ) -> Result<DeferredToolResult, HaiderError> {
        loop {
            let record = self
                .hub
                .delegation(ticket.manifest.agent.clone())
                .await?
                .ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "deferred ticket has no durable delegation",
                        false,
                    )
                })?;
            if let Some(report) = record.report {
                let chip = if report.verified == ReportVerification::Red {
                    ChipState::Error
                } else {
                    ChipState::Done
                };
                return Ok(DeferredToolResult {
                    report,
                    chip,
                    truncated: false,
                });
            }
            if let Some(completion) = self.derive_terminal_report(&record).await? {
                let stored = self
                    .hub
                    .record_delegation_report(record.agent_id.clone(), completion.report)
                    .await?;
                let report = stored.report.ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "reported delegation has no report body",
                        false,
                    )
                })?;
                return Ok(DeferredToolResult {
                    report,
                    chip: completion.chip,
                    truncated: completion.truncated,
                });
            }
            let progress = self.delegation_progress(&record).await?;
            if !progress.input_required {
                match progress.nudge {
                    None if deadline_elapsed(progress.latest_at_ms, self.stall_deadline) => {
                        self.nudge(&record).await?;
                    }
                    Some((_, nudge_at_ms))
                        if deadline_elapsed(
                            progress.latest_at_ms.max(nudge_at_ms),
                            self.stall_deadline,
                        ) =>
                    {
                        self.cancel_subtree(&record, CancelCause::Stall).await?;
                    }
                    _ => {}
                }
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.cancel_subtree(&record, CancelCause::Parent).await?;
                    return Err(HaiderError::new(
                        ErrorCode::RunNotActive,
                        "parent cancelled while waiting for local child",
                        false,
                    ));
                }
                () = tokio::time::sleep(CHILD_POLL_INTERVAL) => {}
            }
        }
    }

    pub(crate) async fn acknowledge(&self, ticket: &DeferredTicket) -> Result<(), HaiderError> {
        self.hub
            .mark_delegation_collected(ticket.manifest.agent.clone())
            .await
            .map(|_| ())
    }

    pub(crate) async fn cancel_ticket(&self, ticket: &DeferredTicket) -> Result<(), HaiderError> {
        let Some(record) = self.hub.delegation(ticket.manifest.agent.clone()).await? else {
            return Ok(());
        };
        self.cancel_subtree(&record, CancelCause::Parent).await
    }

    pub(crate) async fn agent_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<AgentId>, HaiderError> {
        Ok(self
            .hub
            .delegation_for_child_session(session_id.clone())
            .await?
            .map(|record| record.agent_id))
    }

    async fn derive_terminal_report(
        &self,
        record: &DelegationRecord,
    ) -> Result<Option<DeferredToolResult>, HaiderError> {
        let mut cursor = 0;
        let mut terminal = None;
        let mut summary = None;
        let mut failure = None;
        loop {
            let page = self
                .hub
                .read_internal_session(&record.child_session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if envelope.run_id.as_ref() != Some(&record.child_run_id) {
                    continue;
                }
                let Ok(payload) =
                    serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload)
                else {
                    continue;
                };
                match payload {
                    haider_protocol::EventPayload::RunState(state) if state.is_terminal() => {
                        terminal = Some(state);
                    }
                    haider_protocol::EventPayload::RunFailed { message, .. } => {
                        failure = Some(message);
                    }
                    haider_protocol::EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::AgentMessage { text },
                        ..
                    }) if !text.trim().is_empty() => summary = Some(text),
                    _ => {}
                }
            }
        }
        let Some(state) = terminal else {
            return Ok(None);
        };
        let (summary, verified, chip) = match state {
            RunState::Done => (
                summary.unwrap_or_else(|| "subagent completed without a text report".into()),
                ReportVerification::Unverified,
                ChipState::Done,
            ),
            RunState::Errored => (
                failure.unwrap_or_else(|| "subagent failed without public failure detail".into()),
                ReportVerification::Red,
                ChipState::Error,
            ),
            RunState::Cancelled => (
                if self.stall_cancel_requested(record).await? {
                    STALL_REPORT_SUMMARY.into()
                } else {
                    "subagent was cancelled before completing its report".into()
                },
                ReportVerification::Red,
                ChipState::Error,
            ),
            _ => unreachable!("terminal state match is exhaustive"),
        };
        let (summary, truncated) = bounded_summary(summary);
        Ok(Some(DeferredToolResult {
            report: ChildReport {
                agent: record.agent_id.clone(),
                summary,
                verified,
                workspace_revision: None,
            },
            chip,
            truncated,
        }))
    }

    async fn spawn_ancestry(
        &self,
        parent_session_id: &SessionId,
        parent_agent_id: Option<&AgentId>,
    ) -> Result<SpawnAncestry, HaiderError> {
        let Some(parent_agent_id) = parent_agent_id else {
            return Ok(SpawnAncestry {
                root_session_id: parent_session_id.clone(),
                depth: 1,
            });
        };
        let parent = self
            .hub
            .delegation(parent_agent_id.clone())
            .await?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "recursive spawn parent has no durable delegation",
                    false,
                )
            })?;
        if parent.child_session_id != *parent_session_id {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "recursive spawn parent does not own the calling session",
                false,
            ));
        }
        let depth = parent.depth.saturating_add(1);
        if depth > RECURSION_DEPTH_LIMIT {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                RECURSION_LIMIT_MESSAGE,
                false,
            ));
        }
        Ok(SpawnAncestry {
            root_session_id: parent.root_session_id,
            depth,
        })
    }

    async fn nudge(&self, record: &DelegationRecord) -> Result<(), HaiderError> {
        let identity = record.agent_id.as_str();
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": record.child_session_id,
            "run_id": record.child_run_id,
            "text": STALL_NUDGE_TEXT,
            "mode": DeliveryMode::Steer,
        }))
        .map_err(internal_serialization)?;
        let accepted = self
            .hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("delegation-stall-nudge-{identity}"),
                request_digest: digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: record.child_session_id.clone(),
                worker_generation: self.hub.worker_generation(),
                run_id: record.child_run_id.clone(),
                agent_id: Some(record.agent_id.clone()),
                text: STALL_NUDGE_TEXT.into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
                queued_event_id: EventId::new(format!("delegation-nudge-queued-{identity}")),
                user_event_id: EventId::new(format!("delegation-nudge-user-{identity}")),
                active_event_id: EventId::new(format!("delegation-nudge-active-{identity}")),
                device_id: self.hub.device_id(),
            })
            .await?;
        if accepted.disposition != haider_core::TurnAdmissionDisposition::SteerPending {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "stall nudge did not bind to the active child run",
                false,
            ));
        }
        // The durable steer is the restart-safe decision. Delivery is a
        // best-effort wake into the already-owned supervisor; if that wake
        // loses to an exit, the grace deadline still advances to cancellation.
        if let Err(error) = self
            .hub
            .submit_internal_nudge(accepted, STALL_NUDGE_TEXT.into())
            .await
        {
            tracing::warn!(agent = %record.agent_id, ?error, "durable stall nudge wake was not delivered");
        }
        Ok(())
    }

    async fn delegation_progress(
        &self,
        record: &DelegationRecord,
    ) -> Result<DelegationProgress, HaiderError> {
        let direct = self.session_progress(record).await?;
        let mut progress = DelegationProgress {
            latest_at_ms: direct.latest_at_ms,
            nudge: direct.nudge,
            input_required: matches!(
                direct.state,
                Some(RunState::InputRequired { .. } | RunState::PermissionRequired { .. })
            ),
        };
        if !matches!(
            direct.state,
            Some(RunState::Waiting {
                reason: haider_protocol::state::WaitReason::LocalChild
            })
        ) {
            return Ok(progress);
        }

        let mut pending = VecDeque::from(
            self.hub
                .delegations_for_parent_run(
                    record.child_session_id.clone(),
                    record.child_run_id.clone(),
                )
                .await?,
        );
        while let Some(descendant) = pending.pop_front() {
            let child = self.session_progress(&descendant).await?;
            progress.latest_at_ms = progress.latest_at_ms.max(child.latest_at_ms);
            progress.input_required |= matches!(
                child.state,
                Some(RunState::InputRequired { .. } | RunState::PermissionRequired { .. })
            );
            if matches!(
                child.state,
                Some(RunState::Waiting {
                    reason: haider_protocol::state::WaitReason::LocalChild
                })
            ) {
                pending.extend(
                    self.hub
                        .delegations_for_parent_run(
                            descendant.child_session_id,
                            descendant.child_run_id,
                        )
                        .await?,
                );
            }
        }
        Ok(progress)
    }

    async fn session_progress(
        &self,
        record: &DelegationRecord,
    ) -> Result<SessionProgress, HaiderError> {
        let mut cursor = 0;
        let mut latest_at_ms = 0;
        let mut state = None;
        let mut nudge = None;
        loop {
            let page = self
                .hub
                .read_internal_session(&record.child_session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                latest_at_ms = latest_at_ms.max(envelope.committed_at_ms);
                if envelope.run_id.as_ref() != Some(&record.child_run_id) {
                    continue;
                }
                let Ok(payload) =
                    serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload)
                else {
                    continue;
                };
                match payload {
                    haider_protocol::EventPayload::RunState(next) => state = Some(next),
                    haider_protocol::EventPayload::UserMessage { text, mode, .. }
                        if text == STALL_NUDGE_TEXT && mode == DeliveryMode::Steer =>
                    {
                        nudge = Some((envelope.seq, envelope.committed_at_ms));
                    }
                    _ => {}
                }
            }
        }
        Ok(SessionProgress {
            latest_at_ms,
            state,
            nudge,
        })
    }

    async fn cancel_subtree(
        &self,
        record: &DelegationRecord,
        cause: CancelCause,
    ) -> Result<(), HaiderError> {
        let mut pending = vec![record.clone()];
        let mut subtree = Vec::new();
        while let Some(current) = pending.pop() {
            pending.extend(
                self.hub
                    .delegations_for_parent_run(
                        current.child_session_id.clone(),
                        current.child_run_id.clone(),
                    )
                    .await?,
            );
            subtree.push(current);
        }
        for current in subtree.into_iter().rev() {
            let child_cause = if current.agent_id == record.agent_id {
                cause
            } else {
                CancelCause::Ancestor
            };
            let command = self.cancellation_command(&current, child_cause)?;
            self.hub.cancel_internal_turn(command).await?;
        }
        Ok(())
    }

    async fn stall_cancel_requested(&self, record: &DelegationRecord) -> Result<bool, HaiderError> {
        let command = self.cancellation_command(record, CancelCause::Stall)?;
        self.hub
            .has_internal_cancel_receipt(
                command.command_id,
                command.request_digest,
                command.request_json,
            )
            .await
    }

    fn cancellation_command(
        &self,
        record: &DelegationRecord,
        cause: CancelCause,
    ) -> Result<TurnCancelCommand, HaiderError> {
        let reason = cause.name();
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": record.child_session_id,
            "run_id": record.child_run_id,
            "reason": reason,
        }))
        .map_err(internal_serialization)?;
        Ok(TurnCancelCommand {
            command_id: format!("delegation-{reason}-cancel-{}", record.agent_id),
            request_digest: digest_bytes(request_json.as_bytes()),
            request_json,
            session_id: record.child_session_id.clone(),
            worker_generation: self.hub.worker_generation(),
            run_id: record.child_run_id.clone(),
            cancelling_event_id: EventId::new(format!(
                "delegation-{reason}-cancelling-{}",
                record.agent_id
            )),
            device_id: self.hub.device_id(),
        })
    }
}

struct SpawnAncestry {
    root_session_id: SessionId,
    depth: u32,
}

struct SessionProgress {
    latest_at_ms: u64,
    state: Option<RunState>,
    nudge: Option<(u64, u64)>,
}

struct DelegationProgress {
    latest_at_ms: u64,
    nudge: Option<(u64, u64)>,
    input_required: bool,
}

#[derive(Clone, Copy)]
enum CancelCause {
    Stall,
    Parent,
    Ancestor,
}

impl CancelCause {
    fn name(self) -> &'static str {
        match self {
            Self::Stall => "stall",
            Self::Parent => "parent",
            Self::Ancestor => "ancestor",
        }
    }
}

fn deadline_elapsed(committed_at_ms: u64, deadline: Duration) -> bool {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let committed_at_ms = u128::from(committed_at_ms);
    now_ms.saturating_sub(committed_at_ms) >= deadline.as_millis()
}

fn stable_digest(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn digest_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn internal_serialization(error: serde_json::Error) -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        format!("cannot encode delegation coordinates: {error}"),
        false,
    )
}

fn bounded_summary(summary: String) -> (String, bool) {
    if summary.len() <= MAX_REPORT_BYTES {
        return (summary, false);
    }
    let mut end = MAX_REPORT_BYTES;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    (summary[..end].to_owned(), true)
}
