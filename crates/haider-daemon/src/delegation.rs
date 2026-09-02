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
    GraphPinCommand, GraphPinOutcome, MenuResolutionCommand, MenuResolutionOutcome,
    SessionCreateCommand, TurnAcceptCommand, TurnCancelCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::agent::{
    AGENT_GRAPH_ROLLUP_EXTENSION_KIND, AgentGraphRollupV1, AgentManifest, AgentMessageDelivery,
    AgentMessageReceipt, AgentMessaged, AgentRole, ChildReport, ChipState, Grant, Placement,
    ReportVerification,
};
use haider_protocol::effect::EffectClass;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::graph::{
    ChildContractRef, ChildGraphAttached, ChildTemplateCacheKey, ChildWorkflowDecision,
    ChildWorkflowTrigger, EvidenceAuthority, EvidenceVerdict, GraphGateKind, GraphPhase,
    GraphStatus, ParentGraphAttempt, child_contract_subject_digest, child_gate_structure,
    decide_child_workflow_with_registry, graph_template, graph_template_digest, reduce_graphs,
    validate_graph_template,
};
use haider_protocol::ids::{
    AgentId, BranchId, EventId, GraphId, ItemId, LeaseId, MenuId, RunId, SessionId,
};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::loom::{LoomGate, LoomWorkflow, parse_pipe};
use haider_protocol::menu::{AnswerVia, Menu, MenuAnswer, MenuScope};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState, WaitReason};
use haider_protocol::task::TaskEventPayload;
use haider_tools::{MessageSubagent, SpawnSubagent};
#[cfg(unix)]
use rustix::fs::{Mode, OFlags};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CHILD_STALL_DEADLINE: Duration = Duration::from_secs(120);
const CHILD_SETTLEMENT_TAIL_TIMEOUT: Duration = Duration::from_secs(1);
// Registry #94 production arithmetic: 120s pre-nudge stall attribution +
// 120s post-nudge grace + 1s cancellation/reap tail = 241s run wait bound.
// Unlike the two blame clocks, this absolute run clock never pauses.
const CHILD_RUN_WAIT_TIMEOUT: Duration = Duration::from_secs(241);
pub(crate) const DELEGATED_MENU_ORIGIN: &str = "delegated-child";
const DELEGATED_WAIT_TIMEOUT_KIND: &str = "delegated_child_wait_timeout";
pub(crate) const DELEGATION_MIRROR_HANDOFF_EXTENSION_KIND: &str =
    "delegation_terminal_mirror_handoff_v1";
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
    run_wait_timeout: Duration,
    settlement_tail_timeout: Duration,
    #[cfg(all(test, unix))]
    stall_deadline_clock: Option<Arc<StallDeadlineTestClock>>,
}

#[cfg(all(test, unix))]
#[derive(Default)]
pub(crate) struct StallDeadlineTestClock {
    now_ms: AtomicU64,
    checked: Mutex<(u64, u64)>,
}

#[cfg(all(test, unix))]
impl StallDeadlineTestClock {
    pub(crate) fn set_now_ms(&self, now_ms: u64) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }

    pub(crate) fn checks_at(&self, expected_ms: u64) -> u64 {
        let checked = self
            .checked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if checked.0 == expected_ms {
            checked.1
        } else {
            0
        }
    }

    fn deadline_elapsed(&self, committed_at_ms: u64, deadline: Duration) -> bool {
        let now_ms = self.now_ms.load(Ordering::SeqCst);
        let elapsed = deadline_elapsed_at(committed_at_ms, deadline, now_ms);
        let mut checked = self
            .checked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if checked.0 == now_ms {
            checked.1 = checked.1.saturating_add(1);
        } else {
            *checked = (now_ms, 1);
        }
        elapsed
    }
}

#[derive(Debug)]
#[must_use = "the terminal child settlement outcome must be handled"]
enum ChildSettlementOutcome {
    Settled,
    TerminalTimedOut(haider_platform::WaitTimeout),
    TailTimedOut(haider_platform::WaitTimeout),
}

fn report_child_settlement_outcome(outcome: ChildSettlementOutcome) {
    match outcome {
        ChildSettlementOutcome::Settled => {}
        ChildSettlementOutcome::TerminalTimedOut(timeout) => eprintln!(
            "haiderd: lifecycle event=child_terminal_wait_timeout operation={} timeout_ms={}",
            timeout.operation(),
            timeout.limit().as_millis()
        ),
        ChildSettlementOutcome::TailTimedOut(timeout) => eprintln!(
            "haiderd: lifecycle event=child_terminal_tail_timeout operation={} timeout_ms={}",
            timeout.operation(),
            timeout.limit().as_millis()
        ),
    }
}

#[derive(Debug, Clone, Copy)]
struct ChildWaitBudget {
    deadline: tokio::time::Instant,
    active_deadline: tokio::time::Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DelegationMirrorHandoffPhase {
    Pending,
    Completed,
}

/// Daemon-private durable journal fact for a cancellation-tail mirror.
///
/// It is stored as a prompt-omitted `TurnItem::Extension` in the child run.
/// Startup recovery retains terminal runs while a pending fact exists, so a
/// daemon crash can never turn the detached live wake into lost work.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DelegationMirrorHandoff {
    pub(crate) handoff_id: String,
    pub(crate) agent: AgentId,
    pub(crate) child_session_id: SessionId,
    pub(crate) child_run_id: RunId,
    pub(crate) deadline_at_ms: u64,
    pub(crate) cancel_cause: String,
    pub(crate) source: String,
    pub(crate) phase: DelegationMirrorHandoffPhase,
}

impl DelegationMirrorHandoff {
    pub(crate) fn from_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        (kind == DELEGATION_MIRROR_HANDOFF_EXTENSION_KIND)
            .then(|| serde_json::from_value(data.clone()).ok())
            .flatten()
    }
}

enum ChildWaitWake {
    Ready {
        record: DelegationRecord,
        completion: DeferredToolResult,
    },
    ParentCancelled(DelegationRecord),
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
    /// B3 — the resolved Loom agent type of a TYPED spawn: the child's grant
    /// narrows to the type's capabilities (least privilege).
    pub(crate) agent_type: Option<haider_protocol::loom::LoomAgentType>,
    /// Provider ceiling resolved by the parent dispatcher before any spawn
    /// side effect. A restrictive bit is persisted and pins the child's first
    /// run, closing a trust-downgrade race between establishment and launch.
    /// Full is intentionally not pre-bound: the child's actually resolved
    /// no-auth account may still require the stricter automatic floor.
    pub(crate) lockdown: bool,
    /// Exact strict variant of the lockdown ceiling. A boolean-only replay
    /// would restore ordinary lockdown and silently re-enable gateway tools.
    pub(crate) auto_hermetic: bool,
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
            run_wait_timeout: CHILD_RUN_WAIT_TIMEOUT,
            settlement_tail_timeout: CHILD_SETTLEMENT_TAIL_TIMEOUT,
            #[cfg(all(test, unix))]
            stall_deadline_clock: None,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_stall_deadline(hub: SessionHub, stall_deadline: Duration) -> Self {
        Self {
            hub,
            stall_deadline,
            run_wait_timeout: CHILD_RUN_WAIT_TIMEOUT,
            settlement_tail_timeout: CHILD_SETTLEMENT_TAIL_TIMEOUT,
            stall_deadline_clock: None,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_stall_deadline_clock(
        hub: SessionHub,
        stall_deadline: Duration,
        stall_deadline_clock: Arc<StallDeadlineTestClock>,
    ) -> Self {
        Self {
            hub,
            stall_deadline,
            run_wait_timeout: CHILD_RUN_WAIT_TIMEOUT,
            settlement_tail_timeout: CHILD_SETTLEMENT_TAIL_TIMEOUT,
            stall_deadline_clock: Some(stall_deadline_clock),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_settlement_tail_timeout(
        hub: SessionHub,
        settlement_tail_timeout: Duration,
    ) -> Self {
        Self {
            hub,
            stall_deadline: CHILD_STALL_DEADLINE,
            run_wait_timeout: CHILD_RUN_WAIT_TIMEOUT,
            settlement_tail_timeout,
            #[cfg(all(test, unix))]
            stall_deadline_clock: None,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_wait_budgets(
        hub: SessionHub,
        stall_deadline: Duration,
        run_wait_timeout: Duration,
        settlement_tail_timeout: Duration,
    ) -> Self {
        Self {
            hub,
            stall_deadline,
            run_wait_timeout,
            settlement_tail_timeout,
            #[cfg(all(test, unix))]
            stall_deadline_clock: None,
        }
    }

    fn stall_deadline_elapsed(&self, committed_at_ms: u64) -> bool {
        #[cfg(all(test, unix))]
        if let Some(clock) = &self.stall_deadline_clock {
            return clock.deadline_elapsed(committed_at_ms, self.stall_deadline);
        }
        deadline_elapsed(committed_at_ms, self.stall_deadline)
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
        let handoff_dir = if coordinates.lockdown {
            let relative =
                Path::new("handoff").join(handoff_session_short(&coordinates.parent_session_id));
            crate::lockdown::global()
                .and_then(|manager| {
                    manager.sandbox_location(&coordinates.metadata.provider, &relative)
                })
                .map_err(|error| HaiderError::new(ErrorCode::Internal, error.to_string(), false))?
        } else {
            self.ensure_handoff_dir(&coordinates.metadata.cwd, &coordinates.parent_session_id)
                .await?
        };
        let identity = stable_digest(&[
            coordinates.parent_session_id.as_str(),
            coordinates.parent_run_id.as_str(),
            &coordinates.call_id,
        ]);
        let agent_id = AgentId::new(format!("agent-{identity}"));
        let child_session_id = SessionId::new(format!("session-child-{identity}"));
        let child_run_id = RunId::new(format!("run-child-{identity}"));
        let lease = LeaseId::new(format!("lease-child-{identity}"));
        let callsign = callsign_from_identity(&identity);
        let registered_workflow = match request.workflow.as_ref() {
            Some(haider_protocol::graph::ChildWorkflowSelector::WorkflowRef(name))
                if graph_template(name).is_none() =>
            {
                self.hub.loom_workflow(name).await?
            }
            _ => None,
        };
        let decision = decide_child_workflow_with_registry(
            request.workflow.as_ref(),
            request.workflow_trigger,
            request.workflow_author,
            registered_workflow.is_some(),
        );
        let mut requested_grant = crate::worker::default_child_grant();
        // B3 — a typed child starts from its TYPE's grant, intersected with
        // the ordinary child ceiling (never wider than an untyped child).
        if let Some(record) = coordinates.agent_type.as_ref() {
            requested_grant = crate::worker::intersect_grant(
                crate::worker::typed_child_grant(record),
                &requested_grant,
            );
        }
        let mut workflow = self
            .prepare_child_workflow(
                &coordinates,
                &request,
                &decision,
                &child_session_id,
                &child_run_id,
                &identity,
                &requested_grant,
                registered_workflow.as_ref(),
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
        let child_lockdown = coordinates.lockdown;
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
            // Additive display coordinate. Older manifests omit it; the
            // child session metadata remains the execution authority.
            "provider": coordinates.metadata.provider,
            "lockdown": child_lockdown,
            "auto_hermetic": coordinates.auto_hermetic,
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
            // Display identity is assigned once from the same durable digest
            // as the agent/session/run ids. It never repeats or interprets
            // the task, and malformed non-digest input produces no callsign.
            callsign,
            model_profile: coordinates.metadata.model.clone(),
            grant: grant.clone(),
            budget_tokens: Some(coordinates.metadata.max_tokens),
            placement: Placement::Local,
            lease,
            fencing_epoch: self.hub.worker_generation(),
            attempt: 0,
            parent: coordinates.parent_agent_id.clone(),
            coordinates: Some(manifest_coordinates),
            // Round 3: the exec scope FREEZES at spawn — durable manifest
            // truth, immune to later registry edits widening a running
            // child's executable set.
            cli_scope: coordinates
                .agent_type
                .as_ref()
                .map(|record| record.clis.clone()),
        };
        manifest.placement.ensure_local()?;
        // Delegated request_input is answered through the projected parent
        // menu, so the child must retain the ordinary Interactive wait until
        // that durable answer is forwarded. Writes and exec remain
        // pre-allowed through the W9b override seam (journaled as ordinary
        // policy `Allow`); spawning a child is itself the standing permission.
        let child_overrides = Some(haider_protocol::session::SessionPermissionOverridesV1 {
            allow_writes: crate::worker::effect_within_grant(&grant, &EffectClass::FsWrite),
            allow_exec: crate::worker::effect_within_grant(&grant, &EffectClass::ProcessExec),
            allow_mobile: false,
            // A child's pre-allow is bounded per-class by its grant ceiling, so
            // it never gets the blanket auto-allow flip: computer/screen access
            // for a subagent must flow deliberately through the grant, not ride
            // in on the parent's auto-allow mode.
            auto_allow: false,
        });
        let child_interaction_mode =
            haider_protocol::session::SessionInteractionModeV1::Interactive;
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
            "interaction_mode": child_interaction_mode,
        }))
        .map_err(internal_serialization)?;
        let create_digest = digest_bytes(create_json.as_bytes());
        self.hub
            .create_internal_session_with_interaction_mode(
                SessionCreateCommand {
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
                },
                child_interaction_mode,
            )
            .await?;

        if let Some(attached) = workflow.as_ref() {
            let pin_request = serde_json::to_string(&serde_json::json!({
                "session_id": attached.child_session_id,
                "graph_id": attached.child_graph_id,
                "template": attached.template,
                "expected_digest": attached.digest,
            }))
            .map_err(internal_serialization)?;
            let pinned = match self
                .hub
                .pin_graph_matching_digest(
                    GraphPinCommand {
                        command_id: format!("delegation-graph-pin-{identity}"),
                        request_digest: digest_bytes(pin_request.as_bytes()),
                        request_json: pin_request,
                        session_id: attached.child_session_id.clone(),
                        worker_generation: self.hub.worker_generation(),
                        graph_id: attached.child_graph_id.clone(),
                        template: attached.template.clone(),
                        device_id: self.hub.device_id(),
                    },
                    attached.digest.clone(),
                )
                .await
                .map_err(hub_graph_error)?
            {
                GraphPinOutcome::Committed { pinned, .. }
                | GraphPinOutcome::IdempotentReplay { pinned } => pinned,
            };
            if pinned.digest != attached.digest {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "exact child workflow pin returned a different digest",
                    false,
                ));
            }
        }

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
        let manifest = record.manifest.clone();
        let persisted_lockdown = manifest
            .coordinates
            .as_ref()
            .and_then(|coordinates| coordinates.get("lockdown"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let persisted_auto_hermetic = manifest
            .coordinates
            .as_ref()
            .and_then(|coordinates| coordinates.get("auto_hermetic"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if persisted_lockdown {
            self.hub
                .bind_lockdown_turn(
                    &record.child_session_id,
                    &record.child_run_id,
                    &coordinates.metadata.provider,
                    crate::auto_hermetic::ProviderLockdownPolicy::from_binding(
                        persisted_lockdown,
                        persisted_auto_hermetic,
                    ),
                )
                .map_err(|error| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        format!("cannot bind child provider ceiling: {error}"),
                        true,
                    )
                })?;
        }
        self.hub
            .notify_roster_session(record.child_session_id.clone());
        if let Some(attached) = workflow {
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
        registered_workflow: Option<&LoomWorkflow>,
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
        let parent_node = parent
            .nodes
            .iter()
            .find(|candidate| candidate.node == node)
            .ok_or_else(|| {
                workflow_rejection(
                    "missing_parent_attempt",
                    "parent graph node has no reduced attempt state",
                )
            })?;
        let parent_attempt_epoch = parent_node.current_attempt.ok_or_else(|| {
            workflow_rejection(
                "missing_parent_attempt",
                "parent graph node has no open node-local attempt",
            )
        })?;
        let slot = parent_node
            .evidence_slots
            .iter()
            .find(|slot| slot.id == parent_slot)
            .ok_or_else(|| {
                workflow_rejection(
                    "unknown_parent_slot",
                    "workflow child named no declared slot on the parent obligation",
                )
            })?;
        let template = graph_template(template_name)
            .or_else(|| {
                registered_workflow
                    .filter(|workflow| workflow.id == template_name)
                    .map(|workflow| workflow.template.clone())
            })
            .ok_or_else(|| {
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
            attempt: parent_attempt_epoch,
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
        run_deadline: Option<tokio::time::Instant>,
    ) -> Result<DeferredToolResult, HaiderError> {
        let mut chip_mirror = self.load_chip_mirror(ticket).await?;
        let initial = self
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
        let budget = self.child_wait_budget(&initial, run_deadline).await?;
        let wait = async {
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
                self.forward_parent_menu_answers(&record, &mut chip_mirror)
                    .await?;
                if chip_mirror.child_run_terminal {
                    if let Some(report) = record.report.clone() {
                        let chip = if report.verified == ReportVerification::Red {
                            ChipState::Error
                        } else {
                            ChipState::Done
                        };
                        return Ok(ChildWaitWake::Ready {
                            record,
                            completion: DeferredToolResult {
                                report,
                                chip,
                                truncated: false,
                            },
                        });
                    }
                    if let Some(completion) = self.derive_terminal_report(&record).await? {
                        let stored = self
                            .hub
                            .record_delegation_report(
                                record.agent_id.clone(),
                                completion.report.clone(),
                            )
                            .await?;
                        let report = stored.report.clone().ok_or_else(|| {
                            HaiderError::new(
                                ErrorCode::StoreCorrupt,
                                "reported delegation has no report body",
                                false,
                            )
                        })?;
                        return Ok(ChildWaitWake::Ready {
                            record: stored,
                            completion: DeferredToolResult {
                                report,
                                chip: completion.chip,
                                truncated: completion.truncated,
                            },
                        });
                    }
                }
                let progress = self.delegation_progress(&record).await?;
                if !progress.human_required {
                    match progress.nudge {
                        None if self.stall_deadline_elapsed(progress.latest_at_ms) => {
                            self.nudge(&record).await?;
                        }
                        Some((_, nudge_at_ms))
                            if self
                                .stall_deadline_elapsed(progress.latest_at_ms.max(nudge_at_ms)) =>
                        {
                            self.cancel_subtree(&record, CancelCause::Stall).await?;
                        }
                        _ => {}
                    }
                }
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        return Ok(ChildWaitWake::ParentCancelled(record));
                    }
                    () = tokio::time::sleep(CHILD_POLL_INTERVAL) => {}
                }
            }
        };
        let active_limit = budget
            .active_deadline
            .saturating_duration_since(tokio::time::Instant::now());
        match haider_platform::bounded_wait("delegated child active wait", active_limit, wait).await
        {
            haider_platform::BoundedWait::Completed(Ok(ChildWaitWake::Ready {
                record,
                completion,
            })) => {
                let settlement = self
                    .mirror_until_child_terminal(&record, &mut chip_mirror, budget.deadline)
                    .await?;
                report_child_settlement_outcome(settlement);
                self.collapse_child_contract(&record, &completion.report)
                    .await?;
                Ok(completion)
            }
            haider_platform::BoundedWait::Completed(Ok(ChildWaitWake::ParentCancelled(record))) => {
                let handoff = self
                    .begin_terminal_mirror_handoff(
                        &record,
                        &budget,
                        CancelCause::Parent,
                        "collector",
                    )
                    .await?;
                let terminal_deadline = instant_from_unix_deadline(handoff.deadline_at_ms);
                let _cancel_issued = self
                    .cancel_subtree_before(&record, CancelCause::Parent, terminal_deadline)
                    .await?;
                let settlement = self
                    .mirror_until_child_terminal(&record, &mut chip_mirror, terminal_deadline)
                    .await?;
                report_child_settlement_outcome(settlement);
                if chip_mirror.child_run_terminal {
                    self.complete_terminal_mirror_handoff(&record, handoff)
                        .await?;
                }
                Err(HaiderError::new(
                    ErrorCode::RunNotActive,
                    "parent cancelled while waiting for local child",
                    false,
                ))
            }
            haider_platform::BoundedWait::Completed(Err(error)) => Err(error),
            haider_platform::BoundedWait::TimedOut(timeout) => {
                let record = self
                    .hub
                    .delegation(ticket.manifest.agent.clone())
                    .await?
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "timed-out deferred ticket has no durable delegation",
                            false,
                        )
                    })?;
                // Registry #94: the pending journal fact is committed before
                // cancellation, and every following phase consumes the one
                // original absolute deadline. No awaited receipt/store work
                // restarts or extends the reserved terminal/reap tail.
                let handoff = self
                    .begin_terminal_mirror_handoff(
                        &record,
                        &budget,
                        CancelCause::Deadline,
                        "collector",
                    )
                    .await?;
                let terminal_deadline = instant_from_unix_deadline(handoff.deadline_at_ms);
                let _cancel_issued = self
                    .cancel_subtree_before(&record, CancelCause::Deadline, terminal_deadline)
                    .await?;
                let settlement = self
                    .mirror_until_child_terminal(&record, &mut chip_mirror, terminal_deadline)
                    .await?;
                report_child_settlement_outcome(settlement);
                if chip_mirror.child_run_terminal {
                    self.complete_terminal_mirror_handoff(&record, handoff)
                        .await?;
                }
                Err(delegated_wait_timeout(timeout, &record))
            }
        }
    }

    async fn child_wait_budget(
        &self,
        record: &DelegationRecord,
        run_deadline: Option<tokio::time::Instant>,
    ) -> Result<ChildWaitBudget, HaiderError> {
        // This is the delegation clock, not the workflow continuation clock:
        // logical provider-request boundaries own the continuation count,
        // while every child wait in every hop consumes this one parent-run
        // deadline and never increments that count. Interactive runs have no
        // explicit client deadline, so their durable delegation budget starts
        // at the parent run's first committed fact:
        // one stall interval before the nudge + one grace interval after it +
        // the terminal/reap tail. Recovery re-derives the same absolute bound
        // from the journal instead of restarting a relative timer.
        let now = tokio::time::Instant::now();
        let started_at_ms = self.parent_run_started_at_ms(record).await?;
        let deadline = anchored_child_wait_deadline(
            now,
            unix_time_ms(),
            started_at_ms,
            self.run_wait_timeout,
            run_deadline,
        );
        let remaining = deadline.saturating_duration_since(now);
        // Registry #94 arithmetic: active + cancellation_tail == remaining.
        // The tail is at most the configured settlement budget and at most a
        // quarter of a short run, so a deadline always reserves a bounded reap
        // without consuming the entire useful child window.
        let cancellation_tail = self
            .settlement_tail_timeout
            .min(remaining.checked_div(4).unwrap_or_default());
        Ok(ChildWaitBudget {
            deadline,
            active_deadline: deadline.checked_sub(cancellation_tail).unwrap_or(now),
        })
    }

    async fn parent_run_started_at_ms(
        &self,
        record: &DelegationRecord,
    ) -> Result<u64, HaiderError> {
        let mut cursor = 0;
        let mut started_at_ms = None;
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
                if envelope.run_id.as_ref() == Some(&record.parent_run_id) {
                    started_at_ms = Some(
                        started_at_ms.map_or(envelope.committed_at_ms, |started: u64| {
                            started.min(envelope.committed_at_ms)
                        }),
                    );
                }
            }
        }
        started_at_ms.ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                "delegated child has no durable parent run start",
                false,
            )
        })
    }

    /// Replays the durable parent decision into the child's ordinary menu CAS.
    /// A crash after the parent answer but before this wake is repaired by the
    /// recovered collector invoking the same deterministic command again.
    async fn forward_parent_menu_answers(
        &self,
        record: &DelegationRecord,
        mirror: &mut ChipMirror,
    ) -> Result<(), HaiderError> {
        if mirror.menu_routes.is_empty() {
            return Ok(());
        }
        loop {
            let page = self
                .hub
                .read_internal_session(&record.parent_session_id, mirror.parent_answer_cursor, 256)
                .await?;
            if page.is_empty() {
                return Ok(());
            }
            mirror.parent_answer_cursor = page
                .last()
                .map_or(mirror.parent_answer_cursor, |envelope| envelope.seq);
            for envelope in page {
                // The child's Hook answer is projected back into this journal;
                // do not feed that projection through the route a second time.
                // Ordinary parent answers intentionally inherit the opening's
                // child agent coordinate, so the deterministic projection id
                // (not `agent_id`) is the discriminator.
                if envelope
                    .event_id
                    .as_str()
                    .starts_with(&format!("delegation-menu-{}-", record.agent_id.as_str()))
                {
                    continue;
                }
                let Ok(haider_protocol::EventPayload::MenuAnswered(answer)) =
                    serde_json::from_value::<haider_protocol::EventPayload>(
                        envelope.payload.clone(),
                    )
                else {
                    continue;
                };
                let Some(route) = mirror.menu_routes.get(&answer.menu) else {
                    continue;
                };
                let child_answer = MenuAnswer {
                    menu: route.child_menu.id.clone(),
                    option_key: answer.option_key,
                    option_index: answer.option_index,
                    value: answer.value,
                    via: AnswerVia::Hook,
                };
                let outcome = self
                    .hub
                    .resolve_hook_menu(MenuResolutionCommand {
                        command_id: format!(
                            "delegation-menu-forward-{}-{}",
                            record.agent_id, envelope.event_id
                        ),
                        session_id: record.child_session_id.clone(),
                        request_seq: route.request_seq,
                        worker_generation: route.worker_generation,
                        allow_prior_generation: true,
                        answer: child_answer,
                        device_id: self.hub.device_id(),
                        input_is_secret_reference: matches!(
                            route.child_menu.kind,
                            haider_protocol::menu::MenuKind::Secret
                        ),
                    })
                    .await;
                match outcome {
                    Ok(
                        MenuResolutionOutcome::Committed { .. }
                        | MenuResolutionOutcome::IdempotentReplay { .. }
                        | MenuResolutionOutcome::AlreadyResolved { .. },
                    ) => {}
                    Err(error)
                        if matches!(
                            error.code,
                            ErrorCode::MenuNotFound | ErrorCode::MenuAlreadyAnswered
                        ) => {}
                    Err(error) => return Err(error),
                }
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

    pub(crate) async fn cancel_ticket(
        &self,
        ticket: &DeferredTicket,
        run_deadline: Option<tokio::time::Instant>,
    ) -> Result<(), HaiderError> {
        let Some(record) = self.hub.delegation(ticket.manifest.agent.clone()).await? else {
            return Ok(());
        };
        let budget = self.child_wait_budget(&record, run_deadline).await?;
        let handoff = self
            .begin_terminal_mirror_handoff(
                &record,
                &budget,
                CancelCause::Parent,
                "cleanup_fallback",
            )
            .await?;
        // This cleanup hook runs inside parent terminalization, so waiting for
        // the child here would deadlock the cancellation handoff. The active
        // collector normally performs the flush. The detached wake is safe to
        // lose because `handoff` is already durable in the child journal and
        // startup recovery replays it until a completion fact is committed.
        let coordinator = self.clone();
        tokio::spawn(async move {
            match coordinator
                .reconcile_terminal_mirror_handoff(record.clone(), handoff)
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(
                        agent = %record.agent_id,
                        ?error,
                        "terminal child metrics flush failed"
                    );
                }
            }
        });
        Ok(())
    }

    /// Reconcile one journal-derived cancellation mirror after startup.
    ///
    /// This method is synchronous for the startup caller: Ready is not
    /// published until the pending durable handoff either settles or spends
    /// the remainder of its original absolute run deadline.
    pub(crate) async fn recover_terminal_mirror_handoff(
        &self,
        record: DelegationRecord,
        handoff: DelegationMirrorHandoff,
    ) -> Result<(), HaiderError> {
        if record.agent_id != handoff.agent
            || record.child_session_id != handoff.child_session_id
            || record.child_run_id != handoff.child_run_id
        {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "delegation mirror handoff does not match its durable delegation",
                false,
            ));
        }
        self.reconcile_terminal_mirror_handoff(record, handoff)
            .await
    }

    async fn reconcile_terminal_mirror_handoff(
        &self,
        record: DelegationRecord,
        handoff: DelegationMirrorHandoff,
    ) -> Result<(), HaiderError> {
        let deadline = instant_from_unix_deadline(handoff.deadline_at_ms);
        let cause = CancelCause::from_name(&handoff.cancel_cause).unwrap_or(CancelCause::Parent);
        let cancel_issued = self.cancel_subtree_before(&record, cause, deadline).await?;
        let ticket = DeferredTicket {
            id: record.agent_id.as_str().to_owned(),
            manifest: record.manifest.clone(),
        };
        let mut mirror = self.load_chip_mirror(&ticket).await?;
        // A zero-remaining recovered deadline still performs one durable
        // reconciliation pass. `bounded_wait(0, ..)` may select its timer
        // before polling the future, but an expired run must still project the
        // terminal facts startup recovery just committed.
        self.mirror_child_chip_states(&record, &mut mirror).await?;
        let settlement = self
            .mirror_until_child_terminal(&record, &mut mirror, deadline)
            .await?;
        let terminal_seen = mirror.child_run_terminal;
        report_child_settlement_outcome(settlement);
        if terminal_seen {
            self.complete_terminal_mirror_handoff(&record, handoff)
                .await?;
        } else if !cancel_issued {
            tracing::warn!(
                agent = %record.agent_id,
                "terminal child cancellation and mirror remain pending for startup recovery"
            );
        }
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
        let parent = match &record.parent_agent_id {
            Some(parent_agent) => self.hub.delegation(parent_agent.clone()).await?,
            None => None,
        };
        Self::validate_turn_start_record(record, parent)
    }

    /// Validates a delegation and its optional durable parent from the exact
    /// turn-start read bundle, avoiding another store-owner dispatch.
    pub(crate) fn validate_turn_start_record(
        record: DelegationRecord,
        parent: Option<DelegationRecord>,
    ) -> Result<Option<DelegationRecord>, HaiderError> {
        crate::worker::validate_grant(&record.manifest.grant)?;
        if let Some(parent_agent) = &record.parent_agent_id {
            let parent = parent
                .ok_or_else(|| grant_state_corrupt("delegated child has no durable parent"))?;
            if &parent.agent_id != parent_agent {
                return Err(grant_state_corrupt(
                    "delegated child bundle resolved the wrong durable parent",
                ));
            }
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
                    haider_protocol::EventPayload::ProcessSignalRecorded(signal)
                        if signal.workspace_revision.is_some() =>
                    {
                        latest_revision = signal.workspace_revision;
                    }
                    haider_protocol::EventPayload::Effect(
                        haider_protocol::effect::EffectPhase::Outcome {
                            workspace_mutation: Some(mutation),
                            ..
                        },
                    ) if mutation.workspace_revision.is_some() => {
                        latest_revision = mutation.workspace_revision;
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
        let (mut summary, verified, chip) = match state {
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
        if record
            .manifest
            .coordinates
            .as_ref()
            .and_then(|coordinates| coordinates.get("lockdown"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let provider = record
                .manifest
                .coordinates
                .as_ref()
                .and_then(|coordinates| coordinates.get("provider"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            summary = format!("[lockdown provider {provider}] {summary}");
        }
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
        let mut last_rollup = None;
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
                        .starts_with(&format!("delegation-menu-{}-", record.agent_id.as_str()))
                    || envelope
                        .event_id
                        .as_str()
                        .starts_with(&format!("delegation-metrics-{}-", record.agent_id.as_str()))
                    || envelope.event_id.as_str().starts_with(&format!(
                        "delegation-graph-rollup-{}-",
                        record.agent_id.as_str()
                    ))
                {
                    projected_events.insert(envelope.event_id.as_str().to_owned());
                }
                if envelope.run_id.as_ref() != Some(&record.parent_run_id)
                    || envelope.branch_id != record.parent_branch_id
                {
                    continue;
                }
                if let Ok(haider_protocol::EventPayload::AgentChipState { agent, chip }) =
                    serde_json::from_value::<haider_protocol::EventPayload>(
                        envelope.payload.clone(),
                    )
                    && agent == record.agent_id
                {
                    last_chip = Some(chip);
                }
                if let Ok(haider_protocol::EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Extension { kind, data },
                    ..
                })) = serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload)
                    && kind == AGENT_GRAPH_ROLLUP_EXTENSION_KIND
                    && let Ok(rollup) = serde_json::from_value::<AgentGraphRollupV1>(data)
                    && rollup.agent == record.agent_id
                {
                    last_rollup = Some(rollup);
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
            parent_answer_cursor: 0,
            projected_events,
            menu_routes: HashMap::new(),
            emit_projections: true,
            last_chip,
            last_rollup,
            graph_envelopes: Vec::new(),
            child_run_terminal: false,
            metrics_folder: SessionFolder::new(&initial_model),
            terminal_idle_seen: false,
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) async fn terminal_mirror_times_out_for_test(
        &self,
        record: &DelegationRecord,
        limit: Duration,
    ) -> Result<bool, HaiderError> {
        let ticket = DeferredTicket {
            id: record.agent_id.as_str().to_owned(),
            manifest: record.manifest.clone(),
        };
        let mut mirror = self.load_chip_mirror(&ticket).await?;
        // This observer intentionally shares the production wait loop while
        // avoiding a second writer racing the active collector in tests.
        mirror.emit_projections = false;
        self.mirror_until_child_terminal(record, &mut mirror, tokio::time::Instant::now() + limit)
            .await
            .map(|outcome| matches!(outcome, ChildSettlementOutcome::TerminalTimedOut(_)))
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
                if graph_reduction_event(&envelope.payload) {
                    mirror.graph_envelopes.push(envelope.clone());
                    prune_graph_envelopes(&mut mirror.graph_envelopes);
                }
                let Ok(payload) = serde_json::from_value::<haider_protocol::EventPayload>(
                    envelope.payload.clone(),
                ) else {
                    continue;
                };
                let graph_boundary = graph_rollup_boundary(&payload);
                // A historical Idle may precede the run being mirrored. It is
                // not evidence that this run settled; only an Idle observed
                // after its durable terminal can close the best-effort tail.
                if mirror.child_run_terminal
                    && matches!(
                        &payload,
                        haider_protocol::EventPayload::SessionState(SessionState::Idle { .. })
                    )
                {
                    mirror.terminal_idle_seen = true;
                }
                let child_run_event = envelope.run_id.as_ref() == Some(&record.child_run_id);
                let terminal_boundary = child_run_event
                    && matches!(
                        &payload,
                        haider_protocol::EventPayload::RunState(state) if state.is_terminal()
                    );
                if terminal_boundary {
                    mirror.child_run_terminal = true;
                }
                if child_run_event {
                    if let haider_protocol::EventPayload::MenuOpened(menu) = &payload {
                        let proxy_menu_id = delegated_menu_id(record, &menu.id);
                        mirror.menu_routes.insert(
                            proxy_menu_id.clone(),
                            ChildMenuRoute {
                                child_menu: menu.clone(),
                                request_seq: envelope.seq,
                                worker_generation: envelope.worker_generation,
                            },
                        );
                    }
                    if let Some(projected) = delegated_menu_payload(record, &payload) {
                        let event_id = format!(
                            "delegation-menu-{}-{}",
                            record.agent_id.as_str(),
                            envelope.seq
                        );
                        if mirror.projected_events.insert(event_id.clone()) {
                            projections.push(child_menu_projection_envelope(
                                record,
                                &event_id,
                                envelope.event_id.clone(),
                                projected,
                                self.hub.device_id(),
                                self.hub.worker_generation(),
                            )?);
                        }
                    }
                    if let haider_protocol::EventPayload::UserMessage { text, .. } = &payload {
                        let event_id = format!(
                            "delegation-prompt-{}-{}",
                            record.agent_id.as_str(),
                            envelope.seq
                        );
                        if mirror.projected_events.insert(event_id.clone()) {
                            projections.push(child_prompt_projection_envelope(
                                record,
                                &event_id,
                                envelope.event_id.clone(),
                                text,
                                self.hub.device_id(),
                                self.hub.worker_generation(),
                            )?);
                        }
                    }
                    if let haider_protocol::EventPayload::RunState(state) = &payload
                        && let Some(chip) = chip_for_run_state(state)
                    {
                        let event_id = format!(
                            "delegation-chip-{}-{}",
                            record.agent_id.as_str(),
                            envelope.seq
                        );
                        if mirror.projected_events.contains(&event_id) {
                            mirror.last_chip = Some(chip);
                        } else if mirror.last_chip.as_ref() != Some(&chip) {
                            projections.push(chip_projection_envelope(
                                record,
                                &event_id,
                                envelope.event_id.clone(),
                                chip.clone(),
                                self.hub.device_id(),
                                self.hub.worker_generation(),
                            )?);
                            mirror.projected_events.insert(event_id);
                            mirror.last_chip = Some(chip);
                        }
                    }
                }
                if graph_boundary || terminal_boundary {
                    'rollup: {
                        let reductions = reduce_graphs(&mirror.graph_envelopes);
                        let Some(status) = rollup_graph_status(&reductions, &payload) else {
                            break 'rollup;
                        };
                        let workflow = self
                            .hub
                            .pinned_loom_workflow(&status.template, &status.digest)
                            .await?;
                        let Some(rollup) = graph_rollup(
                            &record.agent_id,
                            status,
                            workflow.as_ref(),
                            mirror.child_run_terminal,
                        ) else {
                            break 'rollup;
                        };
                        if !rollup_is_material(&payload, mirror.last_rollup.as_ref(), &rollup) {
                            break 'rollup;
                        }
                        let event_id = format!(
                            "delegation-graph-rollup-{}-{}",
                            record.agent_id.as_str(),
                            envelope.seq
                        );
                        if mirror.projected_events.contains(&event_id) {
                            mirror.last_rollup = Some(rollup);
                        } else if !same_rollup_transition(mirror.last_rollup.as_ref(), &rollup) {
                            projections.push(graph_rollup_projection_envelope(
                                record,
                                &event_id,
                                envelope.event_id,
                                rollup.clone(),
                                self.hub.device_id(),
                                self.hub.worker_generation(),
                            )?);
                            mirror.projected_events.insert(event_id);
                            mirror.last_rollup = Some(rollup);
                        }
                    }
                }
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
            if !mirror.emit_projections {
                projections.clear();
            }
            if !projections.is_empty() {
                self.hub.append(&mut projections).await?;
            }
            mirror.child_cursor = next_cursor;
        }
    }

    /// Resolve on the durable terminal run state. The following session-idle
    /// metrics fence is valuable but cannot retain the parent indefinitely.
    async fn mirror_until_child_terminal(
        &self,
        record: &DelegationRecord,
        mirror: &mut ChipMirror,
        deadline: tokio::time::Instant,
    ) -> Result<ChildSettlementOutcome, HaiderError> {
        let terminal = async {
            loop {
                self.mirror_child_chip_states(record, mirror).await?;
                if mirror.child_run_terminal {
                    return Ok::<(), HaiderError>(());
                }
                tokio::time::sleep(CHILD_POLL_INTERVAL).await;
            }
        };
        let terminal_limit = deadline.saturating_duration_since(tokio::time::Instant::now());
        match haider_platform::bounded_wait(
            "delegated child terminal mirror",
            terminal_limit,
            terminal,
        )
        .await
        {
            haider_platform::BoundedWait::Completed(result) => result?,
            haider_platform::BoundedWait::TimedOut(timeout) => {
                return Ok(ChildSettlementOutcome::TerminalTimedOut(timeout));
            }
        }

        let tail_limit = self
            .settlement_tail_timeout
            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        let tail = async {
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
                    return Ok::<(), HaiderError>(());
                }
                tokio::time::sleep(CHILD_POLL_INTERVAL).await;
            }
        };
        match haider_platform::bounded_wait("terminal child idle/metrics tail", tail_limit, tail)
            .await
        {
            haider_platform::BoundedWait::Completed(result) => {
                result?;
                Ok(ChildSettlementOutcome::Settled)
            }
            haider_platform::BoundedWait::TimedOut(timeout) => {
                Ok(ChildSettlementOutcome::TailTimedOut(timeout))
            }
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

    async fn cancel_subtree_before(
        &self,
        record: &DelegationRecord,
        cause: CancelCause,
        deadline: tokio::time::Instant,
    ) -> Result<bool, HaiderError> {
        let limit = deadline.saturating_duration_since(tokio::time::Instant::now());
        if limit.is_zero() {
            if self
                .session_progress(record)
                .await?
                .state
                .is_some_and(|state| state.is_terminal())
            {
                return Ok(true);
            }
            // Tokio may select a zero-duration timeout before polling the
            // cancellation future. The pending mirror fact is already
            // durable, so enqueue the actor's persist-before-wake command
            // without extending or pausing the exhausted parent deadline.
            self.hub
                .try_cancel_internal_turn(self.cancellation_command(record, cause)?)?;
            return Ok(true);
        }
        match haider_platform::bounded_wait(
            "delegated child cancellation handoff",
            limit,
            self.cancel_subtree(record, cause),
        )
        .await
        {
            haider_platform::BoundedWait::Completed(result) => {
                result?;
                Ok(true)
            }
            haider_platform::BoundedWait::TimedOut(timeout) => {
                eprintln!(
                    "haiderd: lifecycle event=child_cancel_handoff_timeout operation={} timeout_ms={}",
                    timeout.operation(),
                    timeout.limit().as_millis()
                );
                Ok(false)
            }
        }
    }

    async fn begin_terminal_mirror_handoff(
        &self,
        record: &DelegationRecord,
        budget: &ChildWaitBudget,
        cause: CancelCause,
        source: &str,
    ) -> Result<DelegationMirrorHandoff, HaiderError> {
        let now = tokio::time::Instant::now();
        let terminal_deadline = budget.deadline.min(now + self.settlement_tail_timeout);
        let terminal_remaining = terminal_deadline.saturating_duration_since(now);
        let deadline_at_ms = unix_time_ms().saturating_add(duration_millis(terminal_remaining));
        let deadline = deadline_at_ms.to_string();
        let handoff_id =
            stable_digest(&[record.agent_id.as_str(), &deadline, cause.name(), source]);
        let handoff = DelegationMirrorHandoff {
            handoff_id,
            agent: record.agent_id.clone(),
            child_session_id: record.child_session_id.clone(),
            child_run_id: record.child_run_id.clone(),
            deadline_at_ms,
            cancel_cause: cause.name().to_owned(),
            source: source.to_owned(),
            phase: DelegationMirrorHandoffPhase::Pending,
        };
        self.append_terminal_mirror_handoff(record, &handoff)
            .await?;
        Ok(handoff)
    }

    async fn complete_terminal_mirror_handoff(
        &self,
        record: &DelegationRecord,
        mut handoff: DelegationMirrorHandoff,
    ) -> Result<(), HaiderError> {
        handoff.phase = DelegationMirrorHandoffPhase::Completed;
        self.append_terminal_mirror_handoff(record, &handoff).await
    }

    async fn append_terminal_mirror_handoff(
        &self,
        record: &DelegationRecord,
        handoff: &DelegationMirrorHandoff,
    ) -> Result<(), HaiderError> {
        let mut envelope = [terminal_mirror_handoff_envelope(
            record,
            handoff,
            self.hub.device_id(),
            self.hub.worker_generation(),
        )?];
        self.hub.append(&mut envelope).await.map(|_| ())
    }

    #[cfg(all(test, unix))]
    pub(crate) async fn begin_terminal_mirror_handoff_for_test(
        &self,
        record: &DelegationRecord,
        limit: Duration,
    ) -> Result<DelegationMirrorHandoff, HaiderError> {
        let now = tokio::time::Instant::now();
        self.begin_terminal_mirror_handoff(
            record,
            &ChildWaitBudget {
                deadline: now + limit,
                active_deadline: now,
            },
            CancelCause::Parent,
            "test_crash_window",
        )
        .await
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
    parent_answer_cursor: u64,
    projected_events: HashSet<String>,
    menu_routes: HashMap<MenuId, ChildMenuRoute>,
    emit_projections: bool,
    last_chip: Option<ChipState>,
    last_rollup: Option<AgentGraphRollupV1>,
    graph_envelopes: Vec<RawEnvelope>,
    child_run_terminal: bool,
    metrics_folder: SessionFolder,
    terminal_idle_seen: bool,
}

struct ChildMenuRoute {
    child_menu: Menu,
    request_seq: u64,
    worker_generation: u64,
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
    Deadline,
}

impl CancelCause {
    fn name(self) -> &'static str {
        match self {
            Self::Stall => "stall",
            Self::Parent => "parent",
            Self::Ancestor => "ancestor",
            Self::Deadline => "deadline",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "stall" => Some(Self::Stall),
            "parent" => Some(Self::Parent),
            "ancestor" => Some(Self::Ancestor),
            "deadline" => Some(Self::Deadline),
            _ => None,
        }
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Returns the earlier of the explicit parent-run deadline and the fallback
/// delegation deadline anchored to the parent's first durable fact.
///
/// Both inputs are absolute run clocks. In particular, `fallback_total` is
/// never applied to the current hop's `now`: a later autonomous workflow hop
/// can only consume the original child-wait budget, never refresh it.
pub(crate) fn anchored_child_wait_deadline(
    now: tokio::time::Instant,
    now_unix_ms: u64,
    parent_started_at_ms: u64,
    fallback_total: Duration,
    run_deadline: Option<tokio::time::Instant>,
) -> tokio::time::Instant {
    let fallback_deadline_ms = parent_started_at_ms.saturating_add(duration_millis(fallback_total));
    let fallback_deadline =
        now + Duration::from_millis(fallback_deadline_ms.saturating_sub(now_unix_ms));
    run_deadline.map_or(fallback_deadline, |deadline| {
        deadline.min(fallback_deadline)
    })
}

fn instant_from_unix_deadline(deadline_at_ms: u64) -> tokio::time::Instant {
    tokio::time::Instant::now()
        + Duration::from_millis(deadline_at_ms.saturating_sub(unix_time_ms()))
}

fn deadline_elapsed(committed_at_ms: u64, deadline: Duration) -> bool {
    deadline_elapsed_at(committed_at_ms, deadline, unix_time_ms())
}

fn deadline_elapsed_at(committed_at_ms: u64, deadline: Duration, now_ms: u64) -> bool {
    now_ms.saturating_sub(committed_at_ms) >= duration_millis(deadline)
}

pub(crate) fn graph_reduction_event(payload: &serde_json::Value) -> bool {
    // rev933d finding 5: reduce_graphs consumes ONLY the graph family
    // (graph_*, todo_graph_attached, evidence_recorded). Menu events were
    // retained but never read — pure unbounded growth under menu traffic.
    payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| {
            kind.starts_with("graph_")
                || kind == "todo_graph_attached"
                || kind == "evidence_recorded"
        })
}

/// Bounds the mirror's reduction history WITHOUT ever dropping a live
/// graph's events (rev933d finding 5). Runs on every push once a soft cap
/// is crossed: reduce once, keep only events of graphs still non-terminal.
/// A terminal graph can never change a future reduction, so its events are
/// safe to drop; a live graph's are not. A pathological single live graph
/// past the HARD cap drops its own oldest events only.
fn prune_graph_envelopes(envelopes: &mut Vec<RawEnvelope>) {
    const SOFT_CAP: usize = 8_192;
    const HARD_CAP: usize = 16_384;
    if envelopes.len() <= SOFT_CAP {
        return;
    }
    let reductions = reduce_graphs(envelopes);
    envelopes.retain(|envelope| match raw_payload_graph_id(&envelope.payload) {
        Some(graph_id) => reductions
            .graph(&haider_protocol::ids::GraphId::new(graph_id))
            .and_then(|reduction| reduction.status.as_ref())
            .is_none_or(|status| !graph_phase_is_terminal(status.phase)),
        None => true,
    });
    if envelopes.len() > HARD_CAP {
        let excess = envelopes.len() - HARD_CAP;
        envelopes.drain(..excess);
    }
}

pub(crate) fn graph_phase_is_terminal(phase: GraphPhase) -> bool {
    matches!(
        phase,
        GraphPhase::Completed
            | GraphPhase::Blocked
            | GraphPhase::Abandoned
            | GraphPhase::Superseded
    )
}

/// Raw top-level `graph_id` peek — the graph_* family all carry it; events
/// without one (menus, attaches keyed differently) are conservatively kept.
fn raw_payload_graph_id(payload: &serde_json::Value) -> Option<&str> {
    payload.get("graph_id").and_then(serde_json::Value::as_str)
}

fn graph_rollup_boundary(payload: &haider_protocol::EventPayload) -> bool {
    matches!(
        payload,
        haider_protocol::EventPayload::GraphAttemptOpened(_)
            | haider_protocol::EventPayload::GraphGateSatisfied(_)
            | haider_protocol::EventPayload::GraphAdvanced(_)
            | haider_protocol::EventPayload::GraphBlocked(_)
            | haider_protocol::EventPayload::GraphCompleted(_)
            | haider_protocol::EventPayload::GraphAbandoned(_)
            | haider_protocol::EventPayload::GraphSuperseded(_)
            | haider_protocol::EventPayload::MenuOpened(haider_protocol::menu::Menu {
                kind: haider_protocol::menu::MenuKind::GraphHumanConfirm { .. },
                ..
            })
    )
}

fn rollup_graph_status<'a>(
    reductions: &'a haider_protocol::graph::GraphReductions,
    payload: &haider_protocol::EventPayload,
) -> Option<&'a GraphStatus> {
    let reduction = match payload {
        haider_protocol::EventPayload::GraphSuperseded(superseded) => {
            reductions.graph(&superseded.old)
        }
        _ => reductions.active(),
    }?;
    reduction.status.as_ref()
}

pub(crate) fn graph_rollup(
    agent: &AgentId,
    status: &GraphStatus,
    workflow: Option<&LoomWorkflow>,
    child_run_terminal: bool,
) -> Option<AgentGraphRollupV1> {
    let terminal = child_run_terminal
        || matches!(
            status.phase,
            GraphPhase::Completed
                | GraphPhase::Blocked
                | GraphPhase::Abandoned
                | GraphPhase::Superseded
        );
    let node = status.current_node.as_ref().or_else(|| {
        terminal
            .then(|| status.nodes.last().map(|node| &node.node))
            .flatten()
    })?;
    let node_index = status.nodes.iter().position(|entry| &entry.node == node)?;
    let node_status = &status.nodes[node_index];
    let node_meta = workflow.and_then(|workflow| {
        workflow
            .meta
            .iter()
            .find(|candidate| candidate.node == *node)
    });
    let human_gate = matches!(node_status.gate, Some(GraphGateKind::HumanConfirm))
        || (node_status.gate.is_none() && node.as_str() == "SHIP");
    let gate_pending = status.phase == GraphPhase::Active
        && human_gate
        && (status.pending_menu.is_some() || !status.pending_menus.is_empty());
    let state = match status.phase {
        GraphPhase::Completed => "complete",
        GraphPhase::Blocked | GraphPhase::Abandoned | GraphPhase::Superseded => "failed",
        GraphPhase::Active if child_run_terminal => "failed",
        GraphPhase::Active if gate_pending => "gate",
        GraphPhase::Active => "running",
    };
    let gate = (state == "gate").then(|| {
        loom_gate_name(workflow, node_meta.map(|meta| meta.source_name.as_str()))
            .unwrap_or_else(|| graph_gate_name(node_status.gate.as_ref(), node))
    });
    Some(AgentGraphRollupV1 {
        agent: agent.clone(),
        workflow_id: workflow.map(|workflow| workflow.id.clone()),
        template_digest: status.digest.clone(),
        state: state.into(),
        node_index: u64::try_from(node_index.saturating_add(1)).unwrap_or(u64::MAX),
        nodes_total: u64::try_from(status.nodes.len()).unwrap_or(u64::MAX),
        nodes_green: u64::try_from(status.nodes.iter().filter(|node| node.satisfied).count())
            .unwrap_or(u64::MAX),
        node_label: Some(
            node_meta.map_or_else(|| node.label().to_owned(), |meta| meta.source_name.clone()),
        ),
        agent_type: node_meta.and_then(|meta| meta.agent_type.clone()),
        gate,
    })
}

fn loom_gate_name(workflow: Option<&LoomWorkflow>, source_name: Option<&str>) -> Option<String> {
    let source_name = source_name?;
    let gate = parse_pipe(&workflow?.source)
        .nodes
        .into_iter()
        .find(|node| node.name == source_name)?
        .gate;
    Some(match gate {
        LoomGate::Cmd => "cmd".into(),
        LoomGate::Ship => "ship".into(),
        LoomGate::AllOf(n) => format!("all-of-{n}"),
        LoomGate::Human => "human".into(),
    })
}

fn graph_gate_name(
    gate: Option<&GraphGateKind>,
    node: &haider_protocol::graph::GraphNodeName,
) -> String {
    match gate {
        Some(GraphGateKind::CommandGreen) => "cmd".into(),
        Some(GraphGateKind::AllOfN { n }) => format!("all-of-{n}"),
        Some(GraphGateKind::HumanConfirm) => "human".into(),
        None if node.as_str() == "SHIP" => "human".into(),
        None if node.as_str() == "VERIFY" => "all-of-3".into(),
        None => "cmd".into(),
    }
}

pub(crate) fn rollup_is_material(
    payload: &haider_protocol::EventPayload,
    previous: Option<&AgentGraphRollupV1>,
    next: &AgentGraphRollupV1,
) -> bool {
    !matches!(
        payload,
        haider_protocol::EventPayload::GraphGateSatisfied(_)
    ) || next.state != "running"
        || previous.is_none_or(|previous| previous.node_index != next.node_index)
}

pub(crate) fn same_rollup_transition(
    previous: Option<&AgentGraphRollupV1>,
    next: &AgentGraphRollupV1,
) -> bool {
    previous.is_some_and(|previous| {
        previous.state == next.state
            && previous.node_index == next.node_index
            && previous.nodes_green == next.nodes_green
            && previous.gate == next.gate
    })
}

pub(crate) fn chip_for_run_state(state: &RunState) -> Option<ChipState> {
    match state {
        RunState::Streaming => Some(ChipState::Streaming),
        RunState::RunningTool => Some(ChipState::Tool),
        RunState::InputRequired { .. } => Some(ChipState::InputRequired),
        RunState::PermissionRequired { .. } => Some(ChipState::PermissionRequired),
        // A child's positively attributed route outage is visible on its
        // parent-owned chip, but does not alter or cancel the parent's own
        // Waiting(LocalChild) run. Other waits describe parent/child workflow
        // coordination and retain the ordinary Thinking projection.
        RunState::Waiting {
            reason: WaitReason::NetworkUnavailable,
        } => Some(ChipState::Waiting),
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

fn terminal_mirror_handoff_envelope(
    record: &DelegationRecord,
    handoff: &DelegationMirrorHandoff,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
) -> Result<RawEnvelope, HaiderError> {
    let phase = match handoff.phase {
        DelegationMirrorHandoffPhase::Pending => "pending",
        DelegationMirrorHandoffPhase::Completed => "completed",
    };
    let identity = format!("delegation-terminal-mirror-{}-{phase}", handoff.handoff_id);
    let payload = serde_json::to_value(haider_protocol::EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(identity.clone()),
        item: TurnItem::Extension {
            kind: DELEGATION_MIRROR_HANDOFF_EXTENSION_KIND.into(),
            data: serde_json::to_value(handoff).map_err(internal_serialization)?,
        },
    }))
    .map_err(internal_serialization)?;
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(identity),
        seq: 0,
        session_id: record.child_session_id.clone(),
        branch_id: None,
        run_id: Some(record.child_run_id.clone()),
        agent_id: Some(record.agent_id.clone()),
        device_id,
        authority_epoch: 0,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
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

fn delegated_menu_id(record: &DelegationRecord, child_menu_id: &MenuId) -> MenuId {
    MenuId::new(format!(
        "delegated-{}",
        stable_digest(&[
            record.agent_id.as_str(),
            record.child_session_id.as_str(),
            child_menu_id.as_str(),
        ])
    ))
}

fn delegated_menu_payload(
    record: &DelegationRecord,
    payload: &haider_protocol::EventPayload,
) -> Option<haider_protocol::EventPayload> {
    match payload {
        haider_protocol::EventPayload::MenuOpened(menu) => {
            let mut menu = menu.clone();
            menu.id = delegated_menu_id(record, &menu.id);
            menu.scope = MenuScope::Subagent {
                agent: record.agent_id.clone(),
            };
            menu.origin = DELEGATED_MENU_ORIGIN.into();
            Some(haider_protocol::EventPayload::MenuOpened(menu))
        }
        haider_protocol::EventPayload::MenuAnswered(answer) => {
            let mut answer = answer.clone();
            answer.menu = delegated_menu_id(record, &answer.menu);
            Some(haider_protocol::EventPayload::MenuAnswered(answer))
        }
        haider_protocol::EventPayload::MenuClosed { menu, reason } => {
            Some(haider_protocol::EventPayload::MenuClosed {
                menu: delegated_menu_id(record, menu),
                reason: *reason,
            })
        }
        _ => None,
    }
}

fn child_menu_projection_envelope(
    record: &DelegationRecord,
    event_id: &str,
    causation_id: EventId,
    payload: haider_protocol::EventPayload,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
) -> Result<RawEnvelope, HaiderError> {
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: record.parent_session_id.clone(),
        branch_id: record.parent_branch_id.clone(),
        run_id: Some(record.parent_run_id.clone()),
        // Existing live clients route a subagent-scoped MenuOpened into this
        // exact chip transcript using the envelope's agent coordinate.
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
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).map_err(internal_serialization)?,
    })
}

pub(crate) fn graph_rollup_projection_envelope(
    record: &DelegationRecord,
    event_id: &str,
    causation_id: EventId,
    rollup: AgentGraphRollupV1,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
) -> Result<RawEnvelope, HaiderError> {
    let item_id = ItemId::new(event_id);
    let payload = serde_json::to_value(haider_protocol::EventPayload::Item(ItemEvent::Completed {
        item_id,
        item: TurnItem::Extension {
            kind: AGENT_GRAPH_ROLLUP_EXTENSION_KIND.into(),
            data: serde_json::to_value(rollup).map_err(internal_serialization)?,
        },
    }))
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

fn delegated_wait_timeout(
    timeout: haider_platform::WaitTimeout,
    record: &DelegationRecord,
) -> HaiderError {
    let limit_ms = duration_millis(timeout.limit());
    let mut error = HaiderError::new(
        ErrorCode::ProviderTimeout,
        format!(
            "delegated child {} did not settle before the run deadline",
            record.agent_id
        ),
        true,
    )
    .with_presentation(
        ErrorPresentation::new(
            "delegated-child-wait-timeout",
            "Delegated child timed out",
            "The child did not finish before this run's wait deadline. It was cancelled and reaped.",
            ErrorScope::Turn,
            [ErrorAction::Retry],
        )
        .with_timeout_budget(limit_ms, limit_ms),
    );
    error.details = Some(serde_json::json!({
        "kind": DELEGATED_WAIT_TIMEOUT_KIND,
        "operation": timeout.operation(),
        "limit_ms": limit_ms,
        "agent": record.agent_id,
        "child_session_id": record.child_session_id,
        "child_run_id": record.child_run_id,
    }));
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(windows)]
fn seed_handoff_dir(workspace: &Path, session_short: &str) -> Result<(), HaiderError> {
    use std::io::Write as _;

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
    let directory = workspace
        .join(".haider")
        .join("handoff")
        .join(session_short);
    std::fs::create_dir_all(&directory)
        .map_err(|error| handoff_windows_io_error(&directory, error))?;
    for ancestor in [
        workspace.join(".haider"),
        workspace.join(".haider").join("handoff"),
        directory.clone(),
    ] {
        let metadata = std::fs::symlink_metadata(&ancestor)
            .map_err(|error| handoff_windows_io_error(&ancestor, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                format!(
                    "cannot prepare ephemeral handoff path {}: path is not a real directory",
                    ancestor.display()
                ),
                false,
            ));
        }
        haider_platform::set_mode(&ancestor, 0o700)
            .map_err(|error| handoff_windows_io_error(&ancestor, error))?;
    }
    let ignore_path = directory.join(".gitignore");
    if std::fs::symlink_metadata(&ignore_path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(HaiderError::new(
            ErrorCode::Internal,
            format!(
                "cannot prepare ephemeral handoff path {}: path is a symlink",
                ignore_path.display()
            ),
            false,
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    haider_platform::configure_file_mode(&mut options, 0o600);
    let mut ignore = options
        .open(&ignore_path)
        .map_err(|error| handoff_windows_io_error(&ignore_path, error))?;
    ignore
        .write_all(HANDOFF_IGNORE)
        .map_err(|error| handoff_windows_io_error(&ignore_path, error))
}

#[cfg(windows)]
fn handoff_windows_io_error(path: &Path, error: std::io::Error) -> HaiderError {
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

const CALLSIGN_PREFIXES: [&str; 16] = [
    "ash", "blue", "coral", "gold", "jade", "lilac", "mint", "navy", "ochre", "plum", "rose",
    "sage", "teal", "umber", "violet", "white",
];
const CALLSIGN_NAMES: [&str; 16] = [
    "ant", "bear", "crane", "dove", "elk", "fox", "gull", "hare", "ibis", "jay", "kite", "lynx",
    "moth", "newt", "owl", "puma",
];

/// Derives a short, speakable display handle from a minted delegation digest.
/// The two words plus six hexadecimal characters retain 32 identity bits. An
/// arbitrary/malformed string has no honest derivation and remains unnamed.
pub(crate) fn callsign_from_identity(identity: &str) -> Option<String> {
    let bytes = identity.as_bytes();
    if bytes.len() != 64
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    let prefix_index = hex_nibble(*bytes.first()?)?;
    let name_index = hex_nibble(*bytes.get(1)?)?;
    let prefix = CALLSIGN_PREFIXES.get(prefix_index)?;
    let name = CALLSIGN_NAMES.get(name_index)?;
    let suffix = identity.get(2..8)?;
    Some(format!("{prefix}-{name}-{suffix}"))
}

fn hex_nibble(byte: u8) -> Option<usize> {
    match byte {
        b'0'..=b'9' => Some(usize::from(byte - b'0')),
        b'a'..=b'f' => Some(usize::from(byte - b'a' + 10)),
        _ => None,
    }
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
