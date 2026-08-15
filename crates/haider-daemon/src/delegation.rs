//! Daemon-owned local delegation coordinator.
//!
//! This is the only cross-session authority reachable from a parent tool.
//! It exposes typed spawn/collect operations, never a raw store or child
//! session address.
//!
//! # Sequential spawn contract
//!
//! The codex responses-lite history contract admits one tool call per provider
//! round and requires that call's result to remain paired with the call in the
//! next request. A spawn round therefore parks in `Waiting(LocalChild)` until
//! the coordinator has the child's report; only then does core journal the
//! tool result and acknowledge collection. Do not "parallelize" this by
//! acknowledging spawn before the report: that would advance provider history
//! without the result paired to the call that created the child.

use crate::session_hub::SessionHub;
use crate::usage_report::SessionFolder;
use haider_core::{
    AcceptedTurn, CancelToken, ChildGraphAttachCommand, ChildTemplateObservationCommand,
    DeferredTicket, DeferredToolResult, DelegationCreateOutcome, DelegationRecord, DelegationState,
    GraphAbandonCommand, GraphAbandonOutcome, GraphEvidenceCommand, GraphEvidenceOutcome,
    GraphPinCommand, GraphPinOutcome, SessionCreateCommand, TurnAcceptCommand, TurnCancelCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::agent::{
    AgentManifest, AgentMessageDelivery, AgentMessageReceipt, AgentMessaged, AgentRole,
    ChildReport, ChipState, Grant, Placement, ReportVerification,
};
use haider_protocol::effect::EffectClass;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::graph::{
    ChildContractRef, ChildGraphAttached, ChildTemplateCacheKey, ChildWorkflowDecision,
    ChildWorkflowTrigger, EvidenceAuthority, EvidenceVerdict, GraphPhase, ParentGraphAttempt,
    child_contract_subject_digest, child_gate_structure, decide_child_workflow, graph_template,
    graph_template_digest, validate_graph_template,
};
use haider_protocol::ids::{
    AgentId, BranchId, EventId, GraphId, ItemId, LeaseId, RunId, SessionId,
};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::task::TaskEventPayload;
use haider_tools::{MessageSubagent, SpawnSubagent};
use rustix::fs::{Mode, OFlags};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CHILD_STALL_DEADLINE: Duration = Duration::from_secs(120);
pub(crate) const RECURSION_DEPTH_LIMIT: u32 = 3;
pub(crate) const RECURSION_LIMIT_MESSAGE: &str = "recursion depth limit";
const STALL_NUDGE_TEXT: &str = "report your status or conclude";
const STALL_REPORT_SUMMARY: &str =
    "subagent stalled after one nudge and was cancelled without further progress";
const MAX_REPORT_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_PREVIEW_CHARS: usize = 200;
const HANDOFF_IGNORE: &[u8] = b"*";

#[derive(Clone)]
pub(crate) struct DelegationHandle {
    hub: SessionHub,
    stall_deadline: Duration,
}

pub(crate) struct SpawnCoordinates {
    pub(crate) parent_session_id: SessionId,
    pub(crate) parent_run_id: RunId,
    pub(crate) parent_branch_id: Option<BranchId>,
    pub(crate) parent_agent_id: Option<AgentId>,
    pub(crate) tool_item_id: ItemId,
    pub(crate) call_id: String,
    /// The CHILD's creation metadata. By default this is the parent's
    /// current metadata verbatim — the child inherits the parent's CURRENT
    /// model pair, including a pair committed by `session.select_model`
    /// earlier in the parent's life. An explicit spawn model selector arrives
    /// here already resolved ([`DelegationHandle::resolve_child_metadata`]);
    /// `establish` never re-resolves.
    pub(crate) metadata: SessionMetadataV1,
}

pub(crate) struct MessageCoordinates {
    pub(crate) parent_session_id: SessionId,
    pub(crate) parent_agent_id: Option<AgentId>,
    /// Receipt-stable identity: a model tool call id or chip-wire command id.
    pub(crate) command_id: String,
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

    /// Resolves the child's metadata from the parent's CURRENT metadata plus
    /// the request's optional model selector (F1).
    ///
    /// Sessions are provider-agnostic, children included: absent selector →
    /// the child inherits the parent's current pair verbatim; a selector
    /// resolves through the ONE `crate::model_select` authority — the same
    /// truth `session.select_model` validates against. The outer `Err` is
    /// infrastructure failure; the inner `Err` is a typed selection refusal
    /// the model can act on (retry with an explicit pair).
    pub(crate) fn resolve_child_metadata(
        &self,
        parent: &SessionMetadataV1,
        request: &SpawnSubagent,
    ) -> Result<Result<SessionMetadataV1, crate::model_select::SelectionRefusal>, HaiderError> {
        let creatable = self.hub.creatable_providers().map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("creatable-provider registry is unavailable: {error}"),
                false,
            )
        })?;
        let summaries = self
            .hub
            .accounts()
            .map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("account facade is unavailable: {error}"),
                    false,
                )
            })?
            .and_then(|facade| facade.management.read())
            .map(|view| view.providers)
            .unwrap_or_default();
        let authority = crate::model_select::ModelSelectionAuthority::new(creatable, summaries);
        Ok(authority
            .resolve_child_selector(
                &parent.provider,
                &parent.model,
                request.model.as_deref(),
                request.provider.as_deref(),
            )
            .map(|(provider, model)| {
                let mut child = parent.clone();
                child.provider = provider;
                child.model = model;
                child
            }))
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
        let handoff_dir = self
            .ensure_handoff_dir(&coordinates.metadata.cwd, &coordinates.parent_session_id)
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
        let decision = decide_child_workflow(
            request.workflow.as_ref(),
            request.workflow_trigger,
            request.workflow_author,
        );
        let mut requested_grant = crate::worker::default_child_grant();
        let mut workflow = self
            .prepare_child_workflow(
                &coordinates,
                &request,
                &decision,
                &child_session_id,
                &child_run_id,
                &identity,
                &requested_grant,
            )
            .await?;
        if workflow.is_some() {
            if !requested_grant
                .tools
                .iter()
                .any(|tool| tool == "graph_evidence")
            {
                requested_grant.tools.push("graph_evidence".into());
            }
            if decision.workflow_author
                && !requested_grant
                    .tools
                    .iter()
                    .any(|tool| tool == "workflow_author")
            {
                requested_grant.tools.push("workflow_author".into());
            }
        }
        let grant = match ancestry.parent_grant.as_ref() {
            Some(parent) => crate::worker::intersect_grant(requested_grant, parent),
            None => requested_grant,
        };
        crate::worker::validate_grant(&grant)?;
        if let Some(attached) = workflow.as_mut() {
            if !grant.tools.iter().any(|tool| tool == "graph_evidence") {
                return Err(workflow_rejection(
                    "insufficient_child_workflow_grant",
                    "effective child grant withholds graph_evidence required by its workflow",
                ));
            }
            if attached.parent_authority == EvidenceAuthority::DaemonVerified
                && !grant
                    .effect_ceiling
                    .iter()
                    .any(|effect| effect == &EffectClass::ProcessExec)
            {
                return Err(workflow_rejection(
                    "child_authority_growth",
                    "effective child grant withholds the daemon proof capability required by its parent slot",
                ));
            }
            if decision.workflow_author && !grant.tools.iter().any(|tool| tool == "workflow_author")
            {
                return Err(workflow_rejection(
                    "insufficient_child_workflow_grant",
                    "effective child grant withholds the gated workflow_author capability",
                ));
            }
            attached.cache_key.effective_grant_digest = child_grant_digest(&grant)?;
            if let Some(cached) = self
                .hub
                .child_template_cache_lookup(attached.cache_key.clone())
                .await?
            {
                if cached.template.name != attached.template || cached.digest != attached.digest {
                    return Err(workflow_rejection(
                        "colliding_child_template_cache",
                        "promoted child template differs from the selected workflow",
                    ));
                }
                attached.cache_hit = true;
            }
        }
        let mut manifest_coordinates = serde_json::json!({
            "parent_session_id": coordinates.parent_session_id,
            "parent_run_id": coordinates.parent_run_id,
            "call_id": coordinates.call_id,
            "tool_item_id": coordinates.tool_item_id,
            // W6d: the chip view attaches to the child directly.
            "child_session_id": child_session_id,
            // S3: this exact coordinate is also the child prompt's
            // authority; clients never reproduce the private hash seam.
            "handoff_dir": handoff_dir.to_string_lossy(),
        });
        if let Some(attached) = &workflow {
            manifest_coordinates["child_graph"] =
                serde_json::to_value(attached).map_err(internal_serialization)?;
        }
        if request.workflow.is_some() {
            manifest_coordinates["workflow_decision"] =
                serde_json::to_value(&decision).map_err(internal_serialization)?;
        }
        let manifest = AgentManifest {
            agent: agent_id.clone(),
            role: AgentRole::Subagent,
            task: request.task.clone(),
            // W6d (owner ask): no neutral SUB-hex — an ABSENT callsign
            // lets the TUI claim the sim's honor-roll roster
            // deterministically from journal order.
            callsign: None,
            model_profile: coordinates.metadata.model.clone(),
            grant: grant.clone(),
            budget_tokens: Some(coordinates.metadata.max_tokens),
            placement: Placement::Local,
            lease,
            fencing_epoch: self.hub.worker_generation(),
            attempt: 0,
            parent: coordinates.parent_agent_id.clone(),
            coordinates: Some(manifest_coordinates),
        };
        manifest.placement.ensure_local()?;
        // OWNER DIRECTIVE (W6d): delegation is AUTOMATIC — a child must
        // never park on a human. Children are created with writes+exec
        // pre-allowed through the W9b override seam (journaled as ordinary
        // policy `Allow`); spawning a child is itself the standing
        // permission. The parent's own surface keeps its Ask defaults.
        let child_overrides = Some(haider_protocol::session::SessionPermissionOverridesV1 {
            allow_writes: crate::worker::effect_within_grant(&grant, &EffectClass::FsWrite),
            allow_exec: crate::worker::effect_within_grant(&grant, &EffectClass::ProcessExec),
        });
        let create_json = serde_json::to_string(&serde_json::json!({
            "cwd": coordinates.metadata.cwd,
            "provider": coordinates.metadata.provider,
            "model": coordinates.metadata.model,
            "max_tokens": coordinates.metadata.max_tokens,
            "permission_overrides": child_overrides,
            "delegation_agent": agent_id,
            // G3 (LE6): the child inherits the parent's CURRENT tuning; the
            // keys join the semantic digest so a same-identity respawn under
            // different tuning is a different command.
            "effort": coordinates.metadata.effort,
            "fast": coordinates.metadata.fast,
            "cache_policy": coordinates.metadata.cache_policy,
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
                permission_overrides: child_overrides,
                effort: coordinates.metadata.effort.clone(),
                fast: coordinates.metadata.fast,
                cache_policy: coordinates.metadata.cache_policy,
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
            parent_branch_id: coordinates.parent_branch_id,
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
        if let Some(attached) = workflow {
            let pin_request = serde_json::to_string(&serde_json::json!({
                "session_id": attached.child_session_id,
                "graph_id": attached.child_graph_id,
                "template": attached.template,
            }))
            .map_err(internal_serialization)?;
            match self
                .hub
                .pin_graph(GraphPinCommand {
                    command_id: format!("delegation-graph-pin-{identity}"),
                    request_digest: digest_bytes(pin_request.as_bytes()),
                    request_json: pin_request,
                    session_id: attached.child_session_id.clone(),
                    worker_generation: self.hub.worker_generation(),
                    graph_id: attached.child_graph_id.clone(),
                    template: attached.template.clone(),
                    device_id: self.hub.device_id(),
                })
                .await
                .map_err(hub_graph_error)?
            {
                GraphPinOutcome::Committed { .. } | GraphPinOutcome::IdempotentReplay { .. } => {}
            }
            let attach_request =
                serde_json::to_string(&attached).map_err(internal_serialization)?;
            self.hub
                .attach_child_graph(ChildGraphAttachCommand {
                    command_id: format!("delegation-graph-attach-{identity}"),
                    request_digest: digest_bytes(attach_request.as_bytes()),
                    request_json: attach_request,
                    session_id: record.parent_session_id.clone(),
                    parent_branch_id: record.parent_branch_id.clone(),
                    worker_generation: self.hub.worker_generation(),
                    attachment: attached,
                    device_id: self.hub.device_id(),
                })
                .await
                .map_err(hub_graph_error)?;
        }
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
                branch_id: None,
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

    #[allow(clippy::too_many_arguments)]
    async fn prepare_child_workflow(
        &self,
        coordinates: &SpawnCoordinates,
        request: &SpawnSubagent,
        decision: &ChildWorkflowDecision,
        child_session_id: &SessionId,
        child_run_id: &RunId,
        identity: &str,
        requested_grant: &Grant,
    ) -> Result<Option<ChildGraphAttached>, HaiderError> {
        let Some(template_name) = decision.template.as_deref() else {
            return Ok(None);
        };
        let parent = self
            .hub
            .graph_status(&coordinates.parent_session_id)
            .await
            .map_err(hub_graph_error)?
            .filter(|status| status.phase == GraphPhase::Active)
            .ok_or_else(|| {
                workflow_rejection(
                    "missing_parent_attempt",
                    "a child workflow requires an active parent graph attempt",
                )
            })?;
        let node = parent.current_node.clone().ok_or_else(|| {
            workflow_rejection(
                "missing_parent_attempt",
                "active parent graph has no open workflow obligation",
            )
        })?;
        if !parent.node_is_ready(&node) {
            return Err(workflow_rejection(
                "missing_parent_attempt",
                "parent graph node is not ready for delegation",
            ));
        }
        let parent_slot = request.parent_slot.as_deref().ok_or_else(|| {
            workflow_rejection(
                "missing_parent_slot",
                "a workflow child must name its single parent evidence slot",
            )
        })?;
        let slot = parent
            .nodes
            .iter()
            .find(|candidate| candidate.node == node)
            .and_then(|candidate| {
                candidate
                    .evidence_slots
                    .iter()
                    .find(|slot| slot.id == parent_slot)
            })
            .ok_or_else(|| {
                workflow_rejection(
                    "unknown_parent_slot",
                    "workflow child named no declared slot on the parent obligation",
                )
            })?;
        let template = graph_template(template_name).ok_or_else(|| {
            workflow_rejection(
                "unknown_child_workflow",
                format!("unknown child workflow template `{template_name}`"),
            )
        })?;
        validate_graph_template(&template)
            .map_err(|error| workflow_rejection("malformed_child_workflow", error.to_string()))?;
        if template.nodes.iter().any(|node| {
            matches!(
                node.gate,
                haider_protocol::graph::GraphGateKind::HumanConfirm
            )
        }) {
            return Err(workflow_rejection(
                "child_human_gate_forbidden",
                "delegated workflows cannot contain a human-confirm gate",
            ));
        }
        if slot.authority == EvidenceAuthority::DaemonVerified {
            let daemon_proof = template.nodes.iter().any(|node| {
                node.verify_slots
                    .iter()
                    .any(|slot| slot.authority == EvidenceAuthority::DaemonVerified)
            });
            let process_granted = requested_grant
                .effect_ceiling
                .iter()
                .any(|effect| effect == &EffectClass::ProcessExec);
            if !daemon_proof || !process_granted {
                return Err(workflow_rejection(
                    "child_authority_growth",
                    "child workflow was not granted the daemon proof capability required by its parent slot",
                ));
            }
        }
        let digest = graph_template_digest(&template);
        let parent_attempt = ParentGraphAttempt {
            graph_id: parent.graph_id,
            node,
            attempt: parent.attempt,
        };
        Ok(Some(ChildGraphAttached {
            parent_run_id: coordinates.parent_run_id.clone(),
            parent_call_id: coordinates.call_id.clone(),
            parent_tool_item_id: coordinates.tool_item_id.clone(),
            parent_attempt,
            parent_slot: parent_slot.to_owned(),
            parent_authority: slot.authority,
            child_session_id: child_session_id.clone(),
            child_run_id: child_run_id.clone(),
            child_graph_id: GraphId::new(format!("graph-child-{identity}")),
            workflow: decision.requested.clone(),
            template: template.name.clone(),
            digest,
            gate_reason: decision.reason.clone(),
            cache_key: ChildTemplateCacheKey {
                task_shape: child_task_shape(decision.trigger),
                effective_grant_digest: child_grant_digest(requested_grant)?,
                gate_structure: child_gate_structure(&template),
            },
            cache_hit: false,
            workflow_author: decision.workflow_author,
        }))
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
        if let Some(record) = self.hub.delegation(ticket.manifest.agent.clone()).await?
            && delegation_child_graph(&record)?.is_some()
        {
            let request_json = r#"{"why":"child launch failed"}"#.to_owned();
            match self
                .hub
                .abandon_graph(GraphAbandonCommand {
                    command_id: format!("delegation-launch-abandon-{}", record.agent_id),
                    request_digest: digest_bytes(request_json.as_bytes()),
                    request_json,
                    session_id: record.child_session_id,
                    worker_generation: self.hub.worker_generation(),
                    why: "child launch failed".into(),
                    device_id: self.hub.device_id(),
                })
                .await
                .map_err(hub_graph_error)?
            {
                GraphAbandonOutcome::Committed { .. }
                | GraphAbandonOutcome::IdempotentReplay { .. } => {}
            }
        }
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
        let mut chip_mirror = self.load_chip_mirror(ticket).await?;
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
            self.mirror_child_chip_states(&record, &mut chip_mirror)
                .await?;
            if let Some(report) = record.report.clone() {
                self.mirror_until_child_terminal(&record, &mut chip_mirror)
                    .await?;
                self.collapse_child_contract(&record, &report).await?;
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
                self.mirror_until_child_terminal(&record, &mut chip_mirror)
                    .await?;
                let stored = self
                    .hub
                    .record_delegation_report(record.agent_id.clone(), completion.report)
                    .await?;
                let report = stored.report.clone().ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "reported delegation has no report body",
                        false,
                    )
                })?;
                self.collapse_child_contract(&stored, &report).await?;
                return Ok(DeferredToolResult {
                    report,
                    chip: completion.chip,
                    truncated: completion.truncated,
                });
            }
            let progress = self.delegation_progress(&record).await?;
            if !progress.human_required {
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
                    self.mirror_until_child_terminal(&record, &mut chip_mirror).await?;
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

    async fn collapse_child_contract(
        &self,
        record: &DelegationRecord,
        report: &ChildReport,
    ) -> Result<(), HaiderError> {
        let Some(attached) = delegation_child_graph(record)? else {
            return Ok(());
        };
        let child = self
            .hub
            .graph_status(&record.child_session_id)
            .await
            .map_err(hub_graph_error)?
            .ok_or_else(|| {
                workflow_rejection(
                    "mismatched_child_provenance",
                    "workflow child report has no attached graph status",
                )
            })?;
        let verdict = match child.phase {
            GraphPhase::Completed => EvidenceVerdict::Green,
            GraphPhase::Abandoned => EvidenceVerdict::Red,
            _ => {
                return Err(workflow_rejection(
                    "non_terminal_child_contract",
                    "workflow child must complete or explicitly abandon its graph before report collapse",
                ));
            }
        };
        let report_bytes = serde_json::to_vec(report).map_err(internal_serialization)?;
        let contract = ChildContractRef {
            child_session_id: record.child_session_id.clone(),
            child_run_id: record.child_run_id.clone(),
            child_graph_id: child.graph_id.clone(),
            report_digest: digest_bytes(&report_bytes),
            workspace_revision: report.workspace_revision.clone(),
        };
        let subject_digest = child_contract_subject_digest(&contract);
        let cache_equivalent = contract.child_graph_id == attached.child_graph_id;
        let request_json = serde_json::to_string(&serde_json::json!({
            "attachment": attached,
            "contract": contract,
            "verdict": verdict,
        }))
        .map_err(internal_serialization)?;
        let command_id = format!(
            "child-contract-collapse-{}",
            stable_digest(&[
                record.parent_session_id.as_str(),
                record.parent_run_id.as_str(),
                &record.call_id,
                contract.child_graph_id.as_str(),
            ])
        );
        match self
            .hub
            .record_graph_evidence(GraphEvidenceCommand {
                command_id,
                request_digest: digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: record.parent_session_id.clone(),
                worker_generation: self.hub.worker_generation(),
                run_id: record.parent_run_id.clone(),
                call_id: record.call_id.clone(),
                graph_id: attached.parent_attempt.graph_id.clone(),
                node: attached.parent_attempt.node.clone(),
                verdict,
                detail: format!(
                    "child workflow {} {}: {}",
                    attached.child_graph_id,
                    if verdict == EvidenceVerdict::Green {
                        "completed"
                    } else {
                        "abandoned"
                    },
                    report.summary
                ),
                slot: Some(attached.parent_slot.clone()),
                subject_digest: Some(subject_digest),
                signal: None,
                workspace_mutation: None,
                child_contract: Some(contract.clone()),
                device_id: self.hub.device_id(),
            })
            .await
            .map_err(hub_graph_error)?
        {
            GraphEvidenceOutcome::Committed { recorded, .. }
            | GraphEvidenceOutcome::IdempotentReplay { recorded } => {
                if verdict == EvidenceVerdict::Green && cache_equivalent {
                    let template = graph_template(&attached.template).ok_or_else(|| {
                        workflow_rejection(
                            "unknown_child_workflow",
                            "attached child template disappeared before cache observation",
                        )
                    })?;
                    self.hub
                        .observe_child_template_success(ChildTemplateObservationCommand {
                            key: attached.cache_key.clone(),
                            parent_session_id: record.parent_session_id.clone(),
                            parent_attempt: attached.parent_attempt.clone(),
                            collapse_evidence_seq: recorded.evidence_seq,
                            child_contract: contract,
                            template,
                            worker_generation: self.hub.worker_generation(),
                            device_id: self.hub.device_id(),
                        })
                        .await
                        .map_err(hub_graph_error)?;
                }
                Ok(())
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
        self.cancel_subtree(&record, CancelCause::Parent).await?;
        // This cleanup hook runs inside parent terminalization, so waiting for
        // the child here would deadlock the cancellation handoff. The active
        // collector normally performs the flush; this detached rebuild is the
        // durable fallback when cancellation drops that collector first.
        let coordinator = self.clone();
        let ticket = ticket.clone();
        tokio::spawn(async move {
            let result = async {
                let mut mirror = coordinator.load_chip_mirror(&ticket).await?;
                coordinator
                    .mirror_until_child_terminal(&record, &mut mirror)
                    .await
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(
                    agent = %record.agent_id,
                    ?error,
                    "terminal child metrics flush failed"
                );
            }
        });
        Ok(())
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

    pub(crate) async fn record_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<DelegationRecord>, HaiderError> {
        let record = self
            .hub
            .delegation_for_child_session(session_id.clone())
            .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        crate::worker::validate_grant(&record.manifest.grant)?;
        if let Some(parent_agent) = &record.parent_agent_id {
            let parent = self
                .hub
                .delegation(parent_agent.clone())
                .await?
                .ok_or_else(|| grant_state_corrupt("delegated child has no durable parent"))?;
            crate::worker::validate_grant(&parent.manifest.grant)?;
            if crate::worker::intersect_grant(record.manifest.grant.clone(), &parent.manifest.grant)
                != record.manifest.grant
            {
                return Err(grant_state_corrupt(
                    "delegated child grant exceeds its durable parent ceiling",
                ));
            }
        }
        Ok(Some(record))
    }

    /// Returns the parent's shared, ephemeral handoff path for a delegated
    /// child session. Root sessions have no child handoff line.
    pub(crate) async fn handoff_dir_for_child_session(
        &self,
        child_session_id: &SessionId,
        workspace: &str,
    ) -> Result<Option<PathBuf>, HaiderError> {
        Ok(self
            .hub
            .delegation_for_child_session(child_session_id.clone())
            .await?
            .map(|record| {
                record
                    .manifest
                    .coordinates
                    .as_ref()
                    .and_then(|coordinates| coordinates.get("handoff_dir"))
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    // Durable delegations created by older daemons lack the
                    // additive coordinate; retain their historical prompt.
                    .unwrap_or_else(|| handoff_dir(workspace, &record.parent_session_id))
            }))
    }

    /// Best-effort cleanup used only after the parent session's durable
    /// deletion transaction commits. Never call this for idle or shutdown.
    pub(crate) async fn cleanup_handoff_for_deleted_parent(
        &self,
        workspace: &str,
        parent_session_id: &SessionId,
    ) {
        let path = handoff_dir(workspace, parent_session_id);
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), ?error, "could not clean ephemeral handoff directory");
            }
        }
    }

    /// Delivers one direct-child message through the same durable STEER seam
    /// as stall supervision, or starts a fresh immediate turn when the child
    /// has no nonterminal run.
    pub(crate) async fn message(
        &self,
        coordinates: MessageCoordinates,
        request: MessageSubagent,
    ) -> Result<AgentMessageReceipt, HaiderError> {
        let record = self
            .hub
            .delegation(request.agent.clone())
            .await?
            .ok_or_else(|| not_owned_child(&request.agent))?;
        if record.parent_session_id != coordinates.parent_session_id
            || record.parent_agent_id != coordinates.parent_agent_id
        {
            return Err(not_owned_child(&request.agent));
        }

        let identity = stable_digest(&[
            coordinates.parent_session_id.as_str(),
            coordinates
                .parent_agent_id
                .as_ref()
                .map_or("root", AgentId::as_str),
            &coordinates.command_id,
            request.agent.as_str(),
        ]);
        if let Some(receipt) = self
            .replayed_message_receipt(&record, &identity, &request.message)
            .await?
        {
            return Ok(receipt);
        }

        let snapshot = self
            .child_session_snapshot(&record.child_session_id)
            .await?;
        let (accepted, delivery, child_run_state) = if let Some((run_id, state)) = snapshot.active {
            let accepted = self
                .accept_child_message(&record, &identity, run_id, &request.message, "steer")
                .await?;
            if accepted.disposition != haider_core::TurnAdmissionDisposition::SteerPending {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "child message did not bind to the active child run",
                    false,
                ));
            }
            (accepted, AgentMessageDelivery::DeliveredSteer, state)
        } else {
            let run_id = RunId::new(format!("run-child-message-{identity}"));
            let accepted = self
                .accept_child_message(&record, &identity, run_id, &request.message, "queued")
                .await?;
            if !matches!(
                accepted.disposition,
                haider_core::TurnAdmissionDisposition::Started
                    | haider_core::TurnAdmissionDisposition::Queued
            ) {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "idle child message did not create a fresh child turn",
                    false,
                ));
            }
            (
                accepted,
                AgentMessageDelivery::DeliveredQueued,
                RunState::Queued,
            )
        };
        let child_message = self
            .child_message_event(&record.child_session_id, &identity)
            .await?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "accepted child message has no durable user event",
                    false,
                )
            })?;
        self.append_parent_message_facts(
            &record,
            &identity,
            &child_message,
            &child_message.text,
            delivery,
        )
        .await?;
        match delivery {
            AgentMessageDelivery::DeliveredSteer => {
                self.hub
                    .submit_internal_nudge(accepted.clone(), request.message)
                    .await?;
            }
            AgentMessageDelivery::DeliveredSubturn => {
                self.hub
                    .submit_internal_subturn(accepted.clone(), request.message)
                    .await?;
            }
            AgentMessageDelivery::DeliveredQueued => {
                self.hub.submit_internal_turn(accepted.clone()).await?;
            }
        }
        Ok(AgentMessageReceipt {
            agent: record.agent_id,
            delivery,
            child_run_id: accepted.run_id,
            child_run_state,
        })
    }

    async fn accept_child_message(
        &self,
        record: &DelegationRecord,
        identity: &str,
        run_id: RunId,
        text: &str,
        path: &str,
    ) -> Result<AcceptedTurn, HaiderError> {
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": record.child_session_id,
            "run_id": run_id,
            "agent": record.agent_id,
            "text": text,
            "mode": DeliveryMode::Steer,
            "delivery_path": path,
        }))
        .map_err(internal_serialization)?;
        self.hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("delegation-message-{path}-{identity}"),
                request_digest: digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: record.child_session_id.clone(),
                worker_generation: self.hub.worker_generation(),
                branch_id: None,
                run_id,
                agent_id: Some(record.agent_id.clone()),
                text: text.to_owned(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
                queued_event_id: EventId::new(format!(
                    "delegation-message-{path}-queued-{identity}"
                )),
                user_event_id: EventId::new(format!("delegation-message-{path}-user-{identity}")),
                active_event_id: EventId::new(format!(
                    "delegation-message-{path}-active-{identity}"
                )),
                device_id: self.hub.device_id(),
            })
            .await
    }

    async fn child_session_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<ChildSessionSnapshot, HaiderError> {
        let mut cursor = 0;
        let mut states = HashMap::<RunId, (RunState, u64)>::new();
        loop {
            let page = self
                .hub
                .read_internal_session(session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                let Some(run_id) = envelope.run_id else {
                    continue;
                };
                if let Ok(haider_protocol::EventPayload::RunState(state)) =
                    serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload)
                {
                    states.insert(run_id, (state, envelope.seq));
                }
            }
        }
        let active = states
            .iter()
            .filter(|(_, (state, _))| !state.is_terminal() && *state != RunState::Cancelling)
            .max_by_key(|(_, (_, seq))| *seq)
            .map(|(run_id, (state, _))| (run_id.clone(), state.clone()));
        Ok(ChildSessionSnapshot {
            active,
            states: states
                .into_iter()
                .map(|(run_id, (state, _))| (run_id, state))
                .collect(),
        })
    }

    async fn child_message_event(
        &self,
        session_id: &SessionId,
        identity: &str,
    ) -> Result<Option<ChildMessageEvent>, HaiderError> {
        let queued_id = format!("delegation-message-queued-user-{identity}");
        let steer_id = format!("delegation-message-steer-user-{identity}");
        let mut cursor = 0;
        loop {
            let page = self
                .hub
                .read_internal_session(session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                return Ok(None);
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if envelope.event_id.as_str() != queued_id && envelope.event_id.as_str() != steer_id
                {
                    continue;
                }
                let Some(run_id) = envelope.run_id.clone() else {
                    return Err(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "child message user event has no run id",
                        false,
                    ));
                };
                let delivery = if envelope.event_id.as_str() == steer_id {
                    AgentMessageDelivery::DeliveredSteer
                } else {
                    AgentMessageDelivery::DeliveredQueued
                };
                let text =
                    match serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload)
                    {
                        Ok(haider_protocol::EventPayload::UserMessage { text, .. }) => text,
                        _ => {
                            return Err(HaiderError::new(
                                ErrorCode::StoreCorrupt,
                                "child message event is not a user message",
                                false,
                            ));
                        }
                    };
                return Ok(Some(ChildMessageEvent {
                    run_id,
                    event_id: envelope.event_id,
                    seq: envelope.seq,
                    delivery,
                    text,
                }));
            }
        }
    }

    async fn replayed_message_receipt(
        &self,
        record: &DelegationRecord,
        identity: &str,
        text: &str,
    ) -> Result<Option<AgentMessageReceipt>, HaiderError> {
        let Some(message) = self
            .child_message_event(&record.child_session_id, identity)
            .await?
        else {
            return Ok(None);
        };
        if message.text != text {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "agent.message command replayed with different semantics",
                false,
            ));
        }
        self.append_parent_message_facts(
            record,
            identity,
            &message,
            &message.text,
            message.delivery,
        )
        .await?;
        let snapshot = self
            .child_session_snapshot(&record.child_session_id)
            .await?;
        let child_run_state = snapshot
            .states
            .get(&message.run_id)
            .cloned()
            .unwrap_or(RunState::Done);
        if !child_run_state.is_terminal() && child_run_state != RunState::Cancelling {
            let accepted = AcceptedTurn {
                session_id: record.child_session_id.clone(),
                run_id: message.run_id.clone(),
                accepted_seq: message.seq,
                worker_generation: self.hub.worker_generation(),
                branch_id: None,
                first_user_turn: false,
                pdf_attachments: Vec::new(),
                disposition: match message.delivery {
                    AgentMessageDelivery::DeliveredSteer => {
                        haider_core::TurnAdmissionDisposition::SteerPending
                    }
                    AgentMessageDelivery::DeliveredSubturn => {
                        haider_core::TurnAdmissionDisposition::SubturnPending
                    }
                    AgentMessageDelivery::DeliveredQueued => {
                        haider_core::TurnAdmissionDisposition::Started
                    }
                },
            };
            match message.delivery {
                AgentMessageDelivery::DeliveredSteer => {
                    self.hub
                        .submit_internal_nudge(accepted, message.text.clone())
                        .await?;
                }
                AgentMessageDelivery::DeliveredSubturn => {
                    self.hub
                        .submit_internal_subturn(accepted, message.text.clone())
                        .await?;
                }
                AgentMessageDelivery::DeliveredQueued => {
                    self.hub.submit_internal_turn(accepted).await?;
                }
            }
        }
        Ok(Some(AgentMessageReceipt {
            agent: record.agent_id.clone(),
            delivery: message.delivery,
            child_run_id: message.run_id,
            child_run_state,
        }))
    }

    async fn append_parent_message_facts(
        &self,
        record: &DelegationRecord,
        identity: &str,
        child_message: &ChildMessageEvent,
        text: &str,
        delivery: AgentMessageDelivery,
    ) -> Result<(), HaiderError> {
        let fact_id = format!("delegation-message-fact-{identity}");
        let projection_id = format!(
            "delegation-prompt-{}-{}",
            record.agent_id.as_str(),
            child_message.seq
        );
        let existing = self.parent_event_ids(record).await?;
        let mut envelopes = Vec::with_capacity(2);
        if !existing.contains(&fact_id) {
            envelopes.push(agent_messaged_envelope(
                record,
                &fact_id,
                child_message.event_id.clone(),
                text,
                delivery,
                self.hub.device_id(),
                self.hub.worker_generation(),
            )?);
        }
        if !existing.contains(&projection_id) {
            envelopes.push(child_prompt_projection_envelope(
                record,
                &projection_id,
                child_message.event_id.clone(),
                text,
                self.hub.device_id(),
                self.hub.worker_generation(),
            )?);
        }
        if !envelopes.is_empty() {
            self.hub.append(&mut envelopes).await?;
        }
        Ok(())
    }

    async fn parent_event_ids(
        &self,
        record: &DelegationRecord,
    ) -> Result<HashSet<String>, HaiderError> {
        let mut cursor = 0;
        let mut ids = HashSet::new();
        loop {
            let page = self
                .hub
                .read_internal_session(&record.parent_session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                return Ok(ids);
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            ids.extend(
                page.into_iter()
                    .map(|envelope| envelope.event_id.as_str().to_owned()),
            );
        }
    }

    async fn ensure_handoff_dir(
        &self,
        workspace: &str,
        parent_session_id: &SessionId,
    ) -> Result<PathBuf, HaiderError> {
        let workspace = PathBuf::from(workspace);
        let short = handoff_session_short(parent_session_id);
        let path = handoff_dir(
            workspace.to_str().ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "workspace path is not valid UTF-8",
                    false,
                )
            })?,
            parent_session_id,
        );
        tokio::task::spawn_blocking(move || seed_handoff_dir(&workspace, &short))
            .await
            .map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("handoff directory task failed: {error}"),
                    true,
                )
            })??;
        Ok(path)
    }

    async fn derive_terminal_report(
        &self,
        record: &DelegationRecord,
    ) -> Result<Option<DeferredToolResult>, HaiderError> {
        let mut cursor = 0;
        let mut terminal = None;
        let mut summary = None;
        let mut failure = None;
        let mut latest_revision = None;
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
                let Ok(payload) = serde_json::from_value::<haider_protocol::EventPayload>(
                    envelope.payload.clone(),
                ) else {
                    if let Some(TaskEventPayload::TaskCompleted(completed)) =
                        TaskEventPayload::from_payload_value(&envelope.payload)
                        && let Some(revision) = completed
                            .workspace_mutation
                            .and_then(|mutation| mutation.workspace_revision)
                    {
                        latest_revision = Some(revision);
                    }
                    continue;
                };
                match payload {
                    haider_protocol::EventPayload::RunState(state) if state.is_terminal() => {
                        terminal = Some(state);
                    }
                    haider_protocol::EventPayload::RunFailed {
                        message,
                        presentation,
                        ..
                    } => {
                        failure =
                            Some(presentation.map_or(message, |safe| {
                                format!("{} — {}", safe.title, safe.detail)
                            }));
                    }
                    haider_protocol::EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::AgentMessage { text },
                        ..
                    }) if !text.trim().is_empty() => summary = Some(text),
                    haider_protocol::EventPayload::ProcessSignalRecorded(signal) => {
                        if signal.workspace_revision.is_some() {
                            latest_revision = signal.workspace_revision;
                        }
                    }
                    haider_protocol::EventPayload::Effect(
                        haider_protocol::effect::EffectPhase::Outcome {
                            workspace_mutation: Some(mutation),
                            ..
                        },
                    ) => {
                        if mutation.workspace_revision.is_some() {
                            latest_revision = mutation.workspace_revision;
                        }
                    }
                    _ => {}
                }
            }
        }
        let Some(state) = terminal else {
            return Ok(None);
        };
        let workflow_graph = if delegation_child_graph(record)?.is_some() {
            self.hub
                .graph_status(&record.child_session_id)
                .await
                .map_err(hub_graph_error)?
        } else {
            None
        };
        let (summary, verified, chip) = match state {
            RunState::Done => {
                let verified = match workflow_graph.as_ref().map(|graph| graph.phase) {
                    Some(GraphPhase::Completed) => ReportVerification::Verified,
                    Some(GraphPhase::Abandoned) => ReportVerification::Red,
                    _ => ReportVerification::Unverified,
                };
                (
                    summary.unwrap_or_else(|| "subagent completed without a text report".into()),
                    verified,
                    ChipState::Done,
                )
            }
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
                workspace_revision: workflow_graph.and(latest_revision),
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
                parent_grant: None,
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
        crate::worker::validate_grant(&parent.manifest.grant)?;
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
            parent_grant: Some(parent.manifest.grant),
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
                branch_id: None,
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
            human_required: matches!(
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
            progress.human_required |= matches!(
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

    async fn load_chip_mirror(&self, ticket: &DeferredTicket) -> Result<ChipMirror, HaiderError> {
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
        let mut cursor = 0;
        let mut projected_events = HashSet::new();
        let mut last_chip = None;
        loop {
            let page = self
                .hub
                .read_internal_session(&record.parent_session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if envelope
                    .event_id
                    .as_str()
                    .starts_with(&format!("delegation-chip-{}-", record.agent_id.as_str()))
                    || envelope
                        .event_id
                        .as_str()
                        .starts_with(&format!("delegation-prompt-{}-", record.agent_id.as_str()))
                    || envelope
                        .event_id
                        .as_str()
                        .starts_with(&format!("delegation-metrics-{}-", record.agent_id.as_str()))
                {
                    projected_events.insert(envelope.event_id.as_str().to_owned());
                }
                if envelope.run_id.as_ref() != Some(&record.parent_run_id)
                    || envelope.branch_id != record.parent_branch_id
                {
                    continue;
                }
                if let Ok(haider_protocol::EventPayload::AgentChipState { agent, chip }) =
                    serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload)
                    && agent == record.agent_id
                {
                    last_chip = Some(chip);
                }
            }
        }
        let initial_model = self
            .hub
            .session_metadata(&record.child_session_id)
            .await?
            .map_or_else(String::new, |metadata| metadata.model);
        Ok(ChipMirror {
            child_cursor: 0,
            projected_events,
            last_chip,
            metrics_folder: SessionFolder::new(&initial_model),
            terminal_idle_seen: false,
        })
    }

    async fn mirror_child_chip_states(
        &self,
        record: &DelegationRecord,
        mirror: &mut ChipMirror,
    ) -> Result<(), HaiderError> {
        loop {
            let page = self
                .hub
                .read_internal_session(&record.child_session_id, mirror.child_cursor, 256)
                .await?;
            if page.is_empty() {
                return Ok(());
            }
            let next_cursor = page
                .last()
                .map_or(mirror.child_cursor, |envelope| envelope.seq);
            let page_causation = page.last().map(|envelope| envelope.event_id.clone());
            let mut projections = Vec::new();
            for envelope in page {
                mirror.metrics_folder.push(&envelope);
                let Ok(payload) =
                    serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload)
                else {
                    continue;
                };
                if matches!(
                    &payload,
                    haider_protocol::EventPayload::SessionState(SessionState::Idle { .. })
                ) {
                    mirror.terminal_idle_seen = true;
                }
                if envelope.run_id.as_ref() != Some(&record.child_run_id) {
                    continue;
                }
                if let haider_protocol::EventPayload::UserMessage { text, .. } = payload {
                    let event_id = format!(
                        "delegation-prompt-{}-{}",
                        record.agent_id.as_str(),
                        envelope.seq
                    );
                    if mirror.projected_events.insert(event_id.clone()) {
                        projections.push(child_prompt_projection_envelope(
                            record,
                            &event_id,
                            envelope.event_id,
                            &text,
                            self.hub.device_id(),
                            self.hub.worker_generation(),
                        )?);
                    }
                    continue;
                }
                let haider_protocol::EventPayload::RunState(state) = payload else {
                    continue;
                };
                let Some(chip) = chip_for_run_state(&state) else {
                    continue;
                };
                let event_id = format!(
                    "delegation-chip-{}-{}",
                    record.agent_id.as_str(),
                    envelope.seq
                );
                if mirror.projected_events.contains(&event_id) {
                    mirror.last_chip = Some(chip);
                    continue;
                }
                if mirror.last_chip.as_ref() == Some(&chip) {
                    continue;
                }
                projections.push(chip_projection_envelope(
                    record,
                    &event_id,
                    envelope.event_id,
                    chip.clone(),
                    self.hub.device_id(),
                    self.hub.worker_generation(),
                )?);
                mirror.projected_events.insert(event_id);
                mirror.last_chip = Some(chip);
            }
            let metrics_event_id = format!(
                "delegation-metrics-{}-{next_cursor}",
                record.agent_id.as_str()
            );
            if !mirror.projected_events.contains(&metrics_event_id)
                && let Some(snapshot) = mirror.metrics_folder.agent_snapshot(
                    &record.child_session_id,
                    Some(&record.agent_id),
                    next_cursor,
                )
                && let Some(causation_id) = page_causation
            {
                projections.push(metrics_projection_envelope(
                    record,
                    &metrics_event_id,
                    causation_id,
                    snapshot,
                    self.hub.device_id(),
                    self.hub.worker_generation(),
                )?);
                mirror.projected_events.insert(metrics_event_id);
            }
            if !projections.is_empty() {
                self.hub.append(&mut projections).await?;
            }
            mirror.child_cursor = next_cursor;
        }
    }

    /// Keep folding through the terminal run state and the following durable
    /// session-idle fence, then publish the settled snapshot at that head.
    async fn mirror_until_child_terminal(
        &self,
        record: &DelegationRecord,
        mirror: &mut ChipMirror,
    ) -> Result<(), HaiderError> {
        loop {
            self.mirror_child_chip_states(record, mirror).await?;
            if mirror.terminal_idle_seen
                && mirror
                    .metrics_folder
                    .agent_snapshot(
                        &record.child_session_id,
                        Some(&record.agent_id),
                        mirror.child_cursor,
                    )
                    .is_some_and(|snapshot| !snapshot.live)
            {
                return Ok(());
            }
            tokio::time::sleep(CHILD_POLL_INTERVAL).await;
        }
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
    parent_grant: Option<Grant>,
}

fn grant_state_corrupt(message: &str) -> HaiderError {
    HaiderError::new(
        ErrorCode::StoreCorrupt,
        format!("{message}; repair or recreate the delegated session before retrying"),
        false,
    )
}

struct SessionProgress {
    latest_at_ms: u64,
    state: Option<RunState>,
    nudge: Option<(u64, u64)>,
}

struct DelegationProgress {
    latest_at_ms: u64,
    nudge: Option<(u64, u64)>,
    human_required: bool,
}

struct ChipMirror {
    child_cursor: u64,
    projected_events: HashSet<String>,
    last_chip: Option<ChipState>,
    metrics_folder: SessionFolder,
    terminal_idle_seen: bool,
}

struct ChildSessionSnapshot {
    active: Option<(RunId, RunState)>,
    states: HashMap<RunId, RunState>,
}

struct ChildMessageEvent {
    run_id: RunId,
    event_id: EventId,
    seq: u64,
    delivery: AgentMessageDelivery,
    text: String,
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

fn chip_for_run_state(state: &RunState) -> Option<ChipState> {
    match state {
        RunState::Streaming => Some(ChipState::Streaming),
        RunState::RunningTool => Some(ChipState::Tool),
        RunState::InputRequired { .. } => Some(ChipState::InputRequired),
        RunState::PermissionRequired { .. } => Some(ChipState::PermissionRequired),
        RunState::Done | RunState::Errored | RunState::Cancelled => None,
        RunState::Queued
        | RunState::Thinking
        | RunState::Waiting { .. }
        | RunState::Retrying { .. }
        | RunState::Compacting
        | RunState::Verifying { .. }
        | RunState::Concluding
        | RunState::EffectOutcomeUnknown
        | RunState::Cancelling => Some(ChipState::Thinking),
    }
}

pub(crate) fn chip_projection_envelope(
    record: &DelegationRecord,
    event_id: &str,
    causation_id: EventId,
    chip: ChipState,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
) -> Result<RawEnvelope, HaiderError> {
    let payload = serde_json::to_value(haider_protocol::EventPayload::AgentChipState {
        agent: record.agent_id.clone(),
        chip,
    })
    .map_err(internal_serialization)?;
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
        causation_id: Some(causation_id),
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    })
}

pub(crate) fn metrics_projection_envelope(
    record: &DelegationRecord,
    event_id: &str,
    causation_id: EventId,
    snapshot: haider_protocol::agent::AgentMetricsSnapshot,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
) -> Result<RawEnvelope, HaiderError> {
    let payload = snapshot
        .to_payload_value()
        .map_err(internal_serialization)?;
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
        causation_id: Some(causation_id),
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    })
}

pub(crate) fn child_prompt_projection_envelope(
    record: &DelegationRecord,
    event_id: &str,
    causation_id: EventId,
    text: &str,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
) -> Result<RawEnvelope, HaiderError> {
    let payload = serde_json::to_value(haider_protocol::EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: Vec::new(),
        mode: DeliveryMode::Steer,
    })
    .map_err(internal_serialization)?;
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: record.parent_session_id.clone(),
        branch_id: record.parent_branch_id.clone(),
        run_id: Some(record.parent_run_id.clone()),
        // Existing chip routing scopes ordinary transcript payloads by the
        // child agent carried on the envelope.
        agent_id: Some(record.agent_id.clone()),
        device_id,
        authority_epoch: 0,
        worker_generation,
        causation_id: Some(causation_id),
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            // Child transcript visibility must never leak child instructions
            // into the parent's provider history.
            prompt: PromptRender::Omit,
        },
        payload,
    })
}

pub(crate) fn agent_messaged_envelope(
    record: &DelegationRecord,
    event_id: &str,
    causation_id: EventId,
    text: &str,
    delivery: AgentMessageDelivery,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
) -> Result<RawEnvelope, HaiderError> {
    let preview = text.chars().take(MAX_MESSAGE_PREVIEW_CHARS).collect();
    let payload = AgentMessaged {
        agent: record.agent_id.clone(),
        preview,
        delivery,
    }
    .to_payload_value()
    .map_err(internal_serialization)?;
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
        causation_id: Some(causation_id),
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    })
}

fn not_owned_child(agent: &AgentId) -> HaiderError {
    let mut error = HaiderError::new(
        ErrorCode::InvalidArgument,
        format!("subagent {agent} is not a direct child of this session"),
        false,
    );
    error.details = Some(serde_json::json!({
        "kind": "not_owned_child",
        "agent": agent,
    }));
    error
}

fn workflow_rejection(kind: &'static str, message: impl Into<String>) -> HaiderError {
    let mut error = HaiderError::new(ErrorCode::InvalidArgument, message, false);
    error.details = Some(serde_json::json!({ "kind": kind }));
    error
}

fn delegation_child_graph(
    record: &DelegationRecord,
) -> Result<Option<ChildGraphAttached>, HaiderError> {
    let Some(value) = record
        .manifest
        .coordinates
        .as_ref()
        .and_then(|coordinates| coordinates.get("child_graph"))
    else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!("delegation child graph marker is malformed: {error}"),
                false,
            )
        })
}

fn hub_graph_error(error: crate::session_hub::SessionHubError) -> HaiderError {
    match error {
        crate::session_hub::SessionHubError::Store(error) => error,
        other => HaiderError::new(ErrorCode::Internal, other.to_string(), false),
    }
}

fn child_task_shape(trigger: Option<ChildWorkflowTrigger>) -> String {
    match trigger {
        Some(ChildWorkflowTrigger::MutationWithIndependentVerification) => "mutation_verify".into(),
        Some(ChildWorkflowTrigger::DependentPhases) => "dependent_phases".into(),
        Some(ChildWorkflowTrigger::FanOut) => "fan_out".into(),
        Some(ChildWorkflowTrigger::DistinctReview) => "distinct_review".into(),
        Some(ChildWorkflowTrigger::CrashRecovery) => "crash_recovery".into(),
        None => "bare".into(),
    }
}

fn child_grant_digest(grant: &Grant) -> Result<String, HaiderError> {
    let mut tools = grant.tools.clone();
    tools.sort();
    tools.dedup();
    let mut effects = grant
        .effect_ceiling
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal_serialization)?;
    effects.sort();
    effects.dedup();
    serde_json::to_vec(&(tools, effects))
        .map(|bytes| digest_bytes(&bytes))
        .map_err(internal_serialization)
}

fn handoff_session_short(session_id: &SessionId) -> String {
    blake3::hash(session_id.as_str().as_bytes()).to_hex()[..16].to_owned()
}

pub(crate) fn handoff_dir(workspace: &str, session_id: &SessionId) -> PathBuf {
    Path::new(workspace)
        .join(".haider")
        .join("handoff")
        .join(handoff_session_short(session_id))
}

fn seed_handoff_dir(workspace: &Path, session_short: &str) -> Result<(), HaiderError> {
    if !workspace.is_absolute()
        || std::fs::canonicalize(workspace)
            .ok()
            .as_deref()
            .is_none_or(|canonical| canonical != workspace)
    {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "handoff workspace must be an existing canonical directory",
            false,
        ));
    }
    let mut directory = rustix::fs::openat(
        rustix::fs::CWD,
        workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| handoff_io_error(workspace, error))?;
    let mut display = workspace.to_path_buf();
    for component in [".haider", "handoff", session_short] {
        display.push(component);
        match rustix::fs::mkdirat(&directory, component, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(handoff_io_error(&display, error)),
        }
        directory = rustix::fs::openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| handoff_io_error(&display, error))?;
    }
    let ignore_path = display.join(".gitignore");
    let ignore = rustix::fs::openat(
        &directory,
        ".gitignore",
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| handoff_io_error(&ignore_path, error))?;
    let mut written = 0;
    while written < HANDOFF_IGNORE.len() {
        match rustix::io::write(&ignore, &HANDOFF_IGNORE[written..]) {
            Ok(0) => {
                return Err(HaiderError::new(
                    ErrorCode::Internal,
                    format!("short write while seeding {}", ignore_path.display()),
                    true,
                ));
            }
            Ok(count) => written = written.saturating_add(count),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(handoff_io_error(&ignore_path, error)),
        }
    }
    Ok(())
}

fn handoff_io_error(path: &Path, error: rustix::io::Errno) -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        format!(
            "cannot prepare ephemeral handoff path {}: {error}",
            path.display()
        ),
        false,
    )
}

pub(crate) fn stable_digest(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

pub(crate) fn digest_bytes(bytes: &[u8]) -> String {
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
