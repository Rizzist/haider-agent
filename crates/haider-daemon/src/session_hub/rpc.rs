//! CHARTER — the connection's request surface: transport in, semantics down.
//!
//! What lives here: [`HubConnection`]'s method handlers — capability and
//! control-attachment policy checks, argument validation, receipt-first
//! command orchestration (R2/R3/R5), workspace validation, and wire
//! error-code mapping. What may NOT live here: durable mutation (the store
//! owns every transaction; the session actor serializes it — actor.rs),
//! delivery pacing (replay.rs), and provider/tool work (`worker.rs`; a
//! request handler hands the manager a COMMITTED acceptance and returns).
//! Requests on one connection are handled inline by the connection task, so
//! nothing here may await provider work — the longest await is one store
//! transaction or one workspace `spawn_blocking`.

use super::*;
use crate::delegation::{DelegationHandle, MessageCoordinates};
use base64::Engine as _;
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::ChipState;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::effect::{EffectClass, EffectIntent, EffectPhase};
use haider_protocol::item::ItemEvent;
use haider_protocol::menu::MenuKind;
use haider_protocol::state::RunState;
use haider_tools::MessageSubagent;
use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ATTACHMENTS_PER_TURN: usize = 5;
const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES_PER_TURN: usize = 16 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES_PER_PDF_TURN: usize = 64 * 1024 * 1024;

/// Profile-vault alias holding the transcription secret (the Deepgram API
/// key). Daemon-internal: clients only ever speak
/// `transcription.secret_get`/`transcription.secret_set` — the alias never
/// crosses the wire. Public to the crate so integration tests can address
/// the same physical vault item the handler wrote.
pub(crate) const TRANSCRIPTION_SECRET_ALIAS: &str = "transcription.deepgram";
/// ADE key ceiling (`DEEPGRAM_MAX_API_KEY_LENGTH`).
const TRANSCRIPTION_SECRET_MAX_LEN: usize = 512;

pub(crate) fn transcription_secret_alias() -> haider_protocol::ids::CredentialAlias {
    haider_protocol::ids::CredentialAlias::new(TRANSCRIPTION_SECRET_ALIAS)
}

struct AttachmentValidationFailure {
    code: &'static str,
    message: String,
    data: Option<ErrorData>,
}

async fn latest_context_footprint(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    through_seq: u64,
) -> Result<Option<ContextFootprint>, HaiderError> {
    Ok(session_summary_truth(store, session_id, through_seq)
        .await?
        .1)
}

/// One sealed-journal replay computing the roster truth a `session.list`
/// summary carries for an UNATTACHED session: the committed main-timeline
/// user-turn count (durable `UserMessage` envelopes not scoped to a
/// subagent) and the latest durable [`ContextFootprint`] snapshot. These
/// are the SAME durable sources the observe surface replays, so a summary
/// never disagrees with observation after attach.
async fn session_summary_truth(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    through_seq: u64,
) -> Result<(u64, Option<ContextFootprint>), HaiderError> {
    let mut since_seq = 0;
    let mut turns = 0_u64;
    let mut latest = None;
    while since_seq < through_seq {
        let page = store.read(session_id, since_seq, REPLAY_PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                return Ok((turns, latest));
            }
            since_seq = envelope.seq;
            advanced = true;
            let agent_scoped = envelope.agent_id.is_some();
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            match payload {
                EventPayload::UserMessage { .. } if !agent_scoped => {
                    turns = turns.saturating_add(1);
                }
                EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                    if let Some(footprint) = ContextFootprint::from_extension_item(&item) {
                        latest = Some(footprint);
                    }
                }
                _ => {}
            }
        }
        if !advanced {
            break;
        }
    }
    Ok((turns, latest))
}

/// Compact direct-agent metrics from the same sealed journal head carried by
/// `SessionSummary`. The live child path publishes the identical shape into
/// the parent journal; this summary copy is the cold/reconnect and `/usage`
/// main-agent fallback.
async fn session_agent_metrics_truth(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    through_seq: u64,
    initial_model: &str,
) -> Result<Option<haider_protocol::agent::AgentMetricsSnapshot>, HaiderError> {
    let mut folder = crate::usage_report::SessionFolder::new(initial_model);
    let mut since_seq = 0;
    while since_seq < through_seq {
        let page = store.read(session_id, since_seq, REPLAY_PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                return Ok(folder.primary_agent_snapshot(session_id, through_seq));
            }
            since_seq = envelope.seq;
            advanced = true;
            folder.push(&envelope);
        }
        if !advanced {
            break;
        }
    }
    Ok(folder.primary_agent_snapshot(session_id, through_seq))
}

struct FleetChildTruth {
    state: haider_rpc::FleetAgentStateWire,
    metrics: Option<haider_protocol::agent::AgentMetricsSnapshot>,
}

/// Reduces one child's exact durable run and direct metrics from the same
/// sealed journal head. Delegation bookkeeping is only the fallback for the
/// launch-crash window; it cannot distinguish failed from cancelled.
async fn fleet_child_truth(
    store: &dyn StoreHandle,
    record: &haider_core::DelegationRecord,
    through_seq: u64,
    initial_model: &str,
) -> Result<FleetChildTruth, HaiderError> {
    let mut folder = crate::usage_report::SessionFolder::new(initial_model);
    let mut latest_state = None;
    let mut since_seq = 0;
    while since_seq < through_seq {
        let page = store
            .read(&record.child_session_id, since_seq, REPLAY_PAGE_SIZE)
            .await?;
        if page.is_empty() {
            break;
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                break;
            }
            since_seq = envelope.seq;
            advanced = true;
            if envelope.run_id.as_ref() == Some(&record.child_run_id)
                && let Ok(EventPayload::RunState(state)) =
                    serde_json::from_value::<EventPayload>(envelope.payload.clone())
            {
                latest_state = Some(state);
            }
            folder.push(&envelope);
        }
        if !advanced {
            break;
        }
    }
    let state = fleet_agent_state(record, latest_state.as_ref());
    let metrics = folder.agent_snapshot(
        &record.child_session_id,
        Some(&record.agent_id),
        through_seq,
    );
    Ok(FleetChildTruth { state, metrics })
}

fn fleet_agent_state(
    record: &haider_core::DelegationRecord,
    state: Option<&RunState>,
) -> haider_rpc::FleetAgentStateWire {
    use haider_rpc::FleetAgentStateWire as FleetState;
    match state {
        Some(RunState::Queued) => FleetState::Queued,
        Some(RunState::Done) => FleetState::Done,
        Some(RunState::Errored) => FleetState::Failed,
        Some(RunState::Cancelled) => FleetState::Cancelled,
        Some(state) if state.is_parked() => FleetState::Waiting,
        Some(_) => FleetState::Live,
        None => match record.state {
            haider_core::DelegationState::Spawned => FleetState::Queued,
            haider_core::DelegationState::Running => FleetState::Live,
            haider_core::DelegationState::Reported | haider_core::DelegationState::Collected => {
                if record.report.as_ref().is_some_and(|report| {
                    report.verified == haider_protocol::agent::ReportVerification::Red
                }) {
                    FleetState::Failed
                } else {
                    FleetState::Done
                }
            }
        },
    }
}

struct FleetFlatNode {
    record: haider_core::DelegationRecord,
    state: haider_rpc::FleetAgentStateWire,
    metrics: Option<haider_protocol::agent::AgentMetricsSnapshot>,
    direct_child_count: u32,
}

fn fleet_snapshot(
    session_id: SessionId,
    generated_at_ms: u64,
    nodes: Vec<FleetFlatNode>,
    truncated: bool,
) -> Result<haider_rpc::SessionFleetSnapshot, HaiderError> {
    let mut states = haider_rpc::FleetStateCountsWire::default();
    let mut max_depth = 0_u32;
    for node in &nodes {
        max_depth = max_depth.max(node.record.depth);
        let count = match node.state {
            haider_rpc::FleetAgentStateWire::Queued => &mut states.queued,
            haider_rpc::FleetAgentStateWire::Live => &mut states.live,
            haider_rpc::FleetAgentStateWire::Waiting => &mut states.waiting,
            haider_rpc::FleetAgentStateWire::Done => &mut states.done,
            haider_rpc::FleetAgentStateWire::Failed => &mut states.failed,
            haider_rpc::FleetAgentStateWire::Cancelled => &mut states.cancelled,
            _ => continue,
        };
        *count = count.saturating_add(1);
    }
    let (metrics, metrics_complete) = fleet_metrics_totals(&nodes, generated_at_ms);
    let rollup = haider_rpc::FleetRollupWire {
        node_count: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
        states,
        max_depth,
        metrics,
        metrics_complete,
        complete: !truncated,
    };

    let mut children_by_parent = HashMap::<SessionId, Vec<usize>>::new();
    let mut wire_nodes = nodes
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            children_by_parent
                .entry(node.record.parent_session_id.clone())
                .or_default()
                .push(index);
            Some(haider_rpc::FleetNodeWire {
                agent_id: node.record.agent_id,
                session_id: node.record.child_session_id,
                callsign: node.record.manifest.callsign,
                task: node.record.task,
                depth: node.record.depth,
                parent_session_id: node.record.parent_session_id,
                parent_agent_id: node.record.parent_agent_id,
                state: node.state,
                metrics: node.metrics,
                folded_children: node.direct_child_count,
                children: Vec::new(),
            })
        })
        .collect::<Vec<_>>();

    fn take_tree(
        index: usize,
        nodes: &mut [Option<haider_rpc::FleetNodeWire>],
        children_by_parent: &HashMap<SessionId, Vec<usize>>,
    ) -> Result<haider_rpc::FleetNodeWire, HaiderError> {
        let mut node = nodes.get_mut(index).and_then(Option::take).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                "bounded fleet graph contains a duplicate node index",
                false,
            )
        })?;
        if let Some(children) = children_by_parent.get(&node.session_id) {
            let mut nested = Vec::with_capacity(children.len());
            for child in children {
                nested.push(take_tree(*child, nodes, children_by_parent)?);
            }
            let returned_children = u32::try_from(nested.len()).unwrap_or(u32::MAX);
            node.folded_children = node
                .folded_children
                .checked_sub(returned_children)
                .ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "bounded fleet graph contains more children than durable relation truth",
                        false,
                    )
                })?;
            node.children = nested;
        }
        Ok(node)
    }

    let root_indices = children_by_parent
        .get(&session_id)
        .cloned()
        .unwrap_or_default();
    let mut roots = Vec::with_capacity(root_indices.len());
    for index in root_indices {
        roots.push(take_tree(index, &mut wire_nodes, &children_by_parent)?);
    }
    Ok(haider_rpc::SessionFleetSnapshot {
        session_id,
        generated_at_ms,
        node_limit: haider_rpc::FLEET_MAX_NODES,
        depth_limit: haider_rpc::FLEET_MAX_DEPTH,
        roots,
        rollup,
        truncated,
    })
}

fn fleet_metrics_totals(
    nodes: &[FleetFlatNode],
    generated_at_ms: u64,
) -> (haider_rpc::FleetMetricsTotalsWire, bool) {
    let metrics_complete = nodes.iter().all(|node| {
        node.metrics
            .as_ref()
            .is_some_and(|snapshot| snapshot.usage.is_some())
    });
    let mut totals = haider_rpc::FleetMetricsTotalsWire::default();
    for snapshot in nodes.iter().filter_map(|node| node.metrics.as_ref()) {
        totals.elapsed_ms = totals.elapsed_ms.saturating_add(
            snapshot
                .terminal_at_ms
                .unwrap_or(generated_at_ms)
                .saturating_sub(snapshot.started_at_ms),
        );
        totals.tool_attempts = totals.tool_attempts.saturating_add(snapshot.tool_attempts);
    }
    if nodes.is_empty() || !metrics_complete {
        return (totals, metrics_complete);
    }

    let mut usage = haider_protocol::agent::AgentUsageMetrics {
        all_lanes_priced: true,
        ..haider_protocol::agent::AgentUsageMetrics::default()
    };
    let mut metered_cost = 0_u64;
    let mut api_cost = 0_u64;
    let mut metered_priced = true;
    let mut api_priced = true;
    for item in nodes
        .iter()
        .filter_map(|node| node.metrics.as_ref())
        .filter_map(|snapshot| snapshot.usage.as_ref())
    {
        usage.logical_input_tokens = usage
            .logical_input_tokens
            .saturating_add(item.logical_input_tokens);
        usage.billed_output_tokens = usage
            .billed_output_tokens
            .saturating_add(item.billed_output_tokens);
        usage.additional_reasoning_tokens = usage
            .additional_reasoning_tokens
            .saturating_add(item.additional_reasoning_tokens);
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(item.cache_read_tokens);
        usage.cache_write_tokens = usage
            .cache_write_tokens
            .saturating_add(item.cache_write_tokens);
        usage.has_metered_lanes |= item.has_metered_lanes;
        usage.has_oauth_lanes |= item.has_oauth_lanes;
        usage.all_lanes_priced &= item.all_lanes_priced;
        if item.has_metered_lanes {
            if let Some(cost) = item.metered_cost_microusd {
                metered_cost = metered_cost.saturating_add(cost);
            } else {
                metered_priced = false;
            }
        }
        if let Some(cost) = item.api_equivalent_cost_microusd {
            api_cost = api_cost.saturating_add(cost);
        } else {
            api_priced = false;
        }
    }
    usage.metered_cost_microusd =
        (usage.has_metered_lanes && metered_priced).then_some(metered_cost);
    usage.api_equivalent_cost_microusd = (usage.all_lanes_priced && api_priced).then_some(api_cost);
    usage.cache_hit_basis_points = nodes
        .iter()
        .filter_map(|node| node.metrics.as_ref())
        .filter_map(|snapshot| snapshot.usage.as_ref())
        .all(|item| item.cache_hit_basis_points.is_some())
        .then(|| {
            usage
                .cache_read_tokens
                .saturating_mul(10_000)
                .checked_div(usage.logical_input_tokens)
                .map_or(0, |cache_hit_basis_points| {
                    u32::try_from(cache_hit_basis_points)
                        .unwrap_or(10_000)
                        .min(10_000)
                })
        });
    totals.usage = Some(usage);
    (totals, metrics_complete)
}

fn fleet_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Projects one replayed truth into the summary's additive wire fields.
///
/// Zero-honesty law: `Some(0)` tokens are reported EXCLUSIVELY for truly
/// empty sessions (no committed user turn and no durable snapshot — zero
/// is then exact). A session with committed turns but no snapshot reports
/// `None`: unknown is never rendered as zero.
fn summary_footprint_fields(
    turns: u64,
    footprint: Option<&ContextFootprint>,
) -> (Option<u64>, Option<ContextFootprintTruth>) {
    match footprint {
        Some(footprint) => (Some(footprint.used_tokens), Some(footprint.truth)),
        None if turns == 0 => (Some(0), Some(ContextFootprintTruth::Exact)),
        None => (None, None),
    }
}

pub(super) fn filter_provider_summaries(
    providers: Vec<haider_rpc::ProviderSummaryWire>,
    provider: Option<&str>,
) -> Vec<haider_rpc::ProviderSummaryWire> {
    providers
        .into_iter()
        .filter(|summary| provider.is_none_or(|provider| summary.provider == provider))
        .collect()
}

#[derive(Debug)]
struct ObservedRun {
    state: RunState,
    seq: u64,
    branch_id: Option<BranchId>,
}

struct ObserveProjection {
    event_limit: usize,
    event_kinds: VecDeque<String>,
    title: Option<String>,
    runs: HashMap<RunId, ObservedRun>,
    menus: BTreeMap<String, haider_rpc::ObserveMenuWire>,
    subagents: BTreeMap<String, haider_rpc::ObserveSubagentWire>,
    footprint: Option<ContextFootprint>,
    main_head_node_id: Option<haider_protocol::ids::NodeId>,
    main_head_seq: u64,
    branches: HashMap<haider_protocol::ids::BranchId, haider_protocol::branch::BranchDescriptor>,
    updated_at_ms: u64,
}

impl ObserveProjection {
    fn new(event_limit: usize) -> Self {
        Self {
            event_limit,
            event_kinds: VecDeque::with_capacity(event_limit),
            title: None,
            runs: HashMap::new(),
            menus: BTreeMap::new(),
            subagents: BTreeMap::new(),
            footprint: None,
            main_head_node_id: None,
            main_head_seq: 0,
            branches: HashMap::new(),
            updated_at_ms: 0,
        }
    }

    fn apply(&mut self, envelope: haider_protocol::envelope::RawEnvelope) {
        self.updated_at_ms = self.updated_at_ms.max(envelope.committed_at_ms);
        if let Some(kind) = envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            && self.event_limit > 0
        {
            if self.event_kinds.len() == self.event_limit {
                self.event_kinds.pop_front();
            }
            self.event_kinds.push_back(kind.to_owned());
        }
        let seq = envelope.seq;
        let branch_id = envelope.branch_id;
        let run_id = envelope.run_id;
        if let Some(created) =
            haider_protocol::branch::BranchCreated::from_payload_value(&envelope.payload)
        {
            self.branches
                .insert(created.branch.branch_id.clone(), created.branch);
            return;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
            return;
        };
        match payload {
            EventPayload::UserMessage { text, .. } if self.title.is_none() => {
                self.title = Some(observe_title(&text));
            }
            EventPayload::RunState(state) => {
                if let Some(run_id) = run_id {
                    self.runs.insert(
                        run_id,
                        ObservedRun {
                            state,
                            seq,
                            branch_id,
                        },
                    );
                }
            }
            EventPayload::MenuOpened(menu) => {
                let kind = observe_menu_kind(&menu.kind);
                let (permission_description, presentation) = match menu.kind {
                    MenuKind::Permission { effect_summary } => (Some(effect_summary), None),
                    MenuKind::ErrorRecovery { presentation, .. } => (None, Some(presentation)),
                    _ => (None, None),
                };
                self.menus.insert(
                    menu.id.as_str().to_owned(),
                    haider_rpc::ObserveMenuWire {
                        kind: kind.into(),
                        title: menu.title,
                        permission_description,
                        presentation,
                    },
                );
            }
            EventPayload::MenuAnswered(answer) => {
                self.menus.remove(answer.menu.as_str());
            }
            EventPayload::MenuClosed { menu, .. } => {
                self.menus.remove(menu.as_str());
            }
            EventPayload::AgentSpawned(manifest) => {
                self.subagents.insert(
                    manifest.agent.as_str().to_owned(),
                    haider_rpc::ObserveSubagentWire {
                        agent_id: manifest.agent,
                        callsign: manifest.callsign,
                        task: manifest.task,
                        state: "thinking".into(),
                    },
                );
            }
            EventPayload::AgentChipState { agent, chip } => {
                let state = observe_chip_state(chip).to_owned();
                self.subagents
                    .entry(agent.as_str().to_owned())
                    .and_modify(|subagent| subagent.state.clone_from(&state))
                    .or_insert(haider_rpc::ObserveSubagentWire {
                        agent_id: agent,
                        callsign: None,
                        task: String::new(),
                        state,
                    });
            }
            EventPayload::AgentReport(report) => {
                if let Some(subagent) = self.subagents.get_mut(report.agent.as_str()) {
                    subagent.state = match report.verified {
                        haider_protocol::agent::ReportVerification::Red => "error",
                        _ => "done",
                    }
                    .into();
                }
            }
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                if let Some(footprint) = ContextFootprint::from_extension_item(&item) {
                    self.footprint = Some(footprint);
                }
            }
            EventPayload::NodeCommitted(node) => {
                if let Some(branch_id) = branch_id {
                    if let Some(branch) = self.branches.get_mut(&branch_id) {
                        branch.head_node_id = node.node;
                        branch.head_seq = seq;
                    }
                } else {
                    self.main_head_node_id = Some(node.node);
                    self.main_head_seq = seq;
                }
            }
            _ => {}
        }
    }

    fn finish(
        self,
        session_id: SessionId,
        head_seq: u64,
        worker_generation: u64,
        metadata: Option<haider_protocol::session::SessionMetadataV1>,
    ) -> haider_rpc::SessionObserveDigest {
        let selected = select_observed_run(&self.runs);
        let run_state = selected.map_or(haider_rpc::ObserveRunStateWire::Idle, |run| {
            observe_run_state(&run.state)
        });
        let active_branch_id = selected.and_then(|run| run.branch_id.clone());
        let title = self.title.unwrap_or_else(|| {
            metadata
                .as_ref()
                .and_then(|metadata| {
                    std::path::Path::new(&metadata.cwd)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(ToOwned::to_owned)
                })
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| session_id.as_str().to_owned())
        });
        // Branch-created facts and branch node commits come from the same
        // sealed journal prefix as every other observation field. Avoid a
        // mutable registry read that could race ahead of `head_seq`.
        let mut branches = self.branches.into_values().collect::<Vec<_>>();
        branches.sort_by_key(|branch| branch.created_seq);
        haider_rpc::SessionObserveDigest {
            session_id,
            head_seq,
            worker_generation,
            metadata,
            title,
            run_state,
            active_branch_id,
            branches,
            main_head_node_id: self.main_head_node_id,
            main_head_seq: self.main_head_seq,
            latest_context_footprint: self.footprint,
            pending_menus: self.menus.into_values().collect(),
            subagents: self.subagents.into_values().collect(),
            updated_at_ms: self.updated_at_ms,
            last_event_kinds: self.event_kinds.into_iter().collect(),
        }
    }
}

fn select_observed_run(runs: &HashMap<RunId, ObservedRun>) -> Option<&ObservedRun> {
    let predicates: [fn(&RunState) -> bool; 4] = [
        |state| matches!(state, RunState::PermissionRequired { .. }),
        |state| matches!(state, RunState::InputRequired { .. }),
        |state| !state.is_terminal() && !matches!(state, RunState::Queued),
        |state| matches!(state, RunState::Queued),
    ];
    for predicate in predicates {
        if let Some(run) = runs
            .values()
            .filter(|run| predicate(&run.state))
            .max_by_key(|run| run.seq)
        {
            return Some(run);
        }
    }
    runs.values().max_by_key(|run| run.seq)
}

fn observe_run_state(state: &RunState) -> haider_rpc::ObserveRunStateWire {
    match state {
        RunState::PermissionRequired { .. } => haider_rpc::ObserveRunStateWire::ParkedPermission,
        RunState::InputRequired { .. } => haider_rpc::ObserveRunStateWire::ParkedInput,
        RunState::Errored => haider_rpc::ObserveRunStateWire::Errored,
        RunState::Cancelled => haider_rpc::ObserveRunStateWire::Cancelled,
        RunState::Done => haider_rpc::ObserveRunStateWire::Idle,
        RunState::Queued
        | RunState::Thinking
        | RunState::Streaming
        | RunState::RunningTool
        | RunState::Waiting { .. }
        | RunState::Retrying { .. }
        | RunState::Compacting
        | RunState::Verifying { .. }
        | RunState::Concluding
        | RunState::EffectOutcomeUnknown
        | RunState::Cancelling => haider_rpc::ObserveRunStateWire::Running,
    }
}

fn observe_menu_kind(kind: &MenuKind) -> &'static str {
    match kind {
        MenuKind::Permission { .. } => "permission",
        MenuKind::Recovery { .. } => "recovery",
        MenuKind::ErrorRecovery { .. } => "error_recovery",
        MenuKind::Exhausted => "exhausted",
        MenuKind::TrustHook => "trust_hook",
        MenuKind::Update => "update",
        MenuKind::Question => "question",
        MenuKind::Choice => "choice",
        MenuKind::Secret => "secret",
        MenuKind::File => "file",
        MenuKind::Conflict => "conflict",
        MenuKind::GraphHumanConfirm { .. } => "graph_human_confirm",
        MenuKind::GraphAbandonConfirm { .. } => "graph_abandon_confirm",
    }
}

fn observe_chip_state(state: ChipState) -> &'static str {
    match state {
        ChipState::Idle => "idle",
        ChipState::Thinking => "thinking",
        ChipState::Streaming => "streaming",
        ChipState::Tool => "tool",
        ChipState::Waiting => "waiting",
        ChipState::InputRequired => "input_required",
        ChipState::PermissionRequired => "permission_required",
        ChipState::Done => "done",
        ChipState::Error => "error",
        ChipState::Closed => "closed",
    }
}

fn observe_title(text: &str) -> String {
    let body = if text.starts_with('/') {
        text.split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        text.to_owned()
    };
    let joined = body
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ");
    let truncated = if joined.chars().count() > 46 {
        let cut = joined.chars().take(46).collect::<String>();
        format!("{}…", cut.trim_end())
    } else {
        joined
    };
    let mut chars = truncated.chars();
    chars.next().map_or_else(
        || "New session".to_owned(),
        |first| first.to_uppercase().collect::<String>() + chars.as_str(),
    )
}

// ─────────── connection RPC surface: list/read/attach/detach/menu ───────────

impl SessionHub {
    /// Executes a read-only check for one ambiguous effect, then reopens the
    /// standard card with the observation. The check intentionally lives
    /// outside the serialized session actor: filesystem/network inspection
    /// may block, while the actor's charter permits store awaits only.
    async fn probe_effect_outcome(
        &self,
        session_id: SessionId,
        effect: haider_protocol::ids::EffectId,
        menu_id: MenuId,
        answer: &RawEnvelope,
    ) -> Result<(), HaiderError> {
        let mut cursor = 0_u64;
        let mut intent = None::<EffectIntent>;
        loop {
            let page = self.inner.store.read(&session_id, cursor, 256).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if envelope.run_id != answer.run_id {
                    continue;
                }
                if let Ok(EventPayload::Effect(EffectPhase::Intent(candidate))) =
                    serde_json::from_value(envelope.payload)
                    && candidate.effect == effect
                {
                    intent = Some(candidate);
                }
            }
        }
        let intent = intent.ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!("recovery effect {effect} has no durable intent"),
                false,
            )
        })?;
        let metadata = self
            .inner
            .store
            .session_metadata(&session_id)
            .await?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "effect probe session metadata is unavailable",
                    false,
                )
            })?;
        let delegation_count = if intent.class == EffectClass::AgentSpawn {
            match answer.run_id.clone() {
                Some(run) => Some(
                    self.inner
                        .store
                        .delegations_for_parent_run(session_id.clone(), run)
                        .await?
                        .len(),
                ),
                None => Some(0),
            }
        } else {
            None
        };
        let observation = effect_probe_observation(&intent, &metadata.cwd, delegation_count).await;
        let probe_menu = effect_recovery_menu(
            MenuId::new(format!("{menu_id}-probe-{}", answer.seq)),
            effect,
            format!("{}; probe result: {observation}", intent.summary),
        );
        let payloads = [
            EventPayload::MenuOpened(probe_menu),
            EventPayload::RunState(RunState::EffectOutcomeUnknown),
        ];
        let mut envelopes = Vec::with_capacity(payloads.len());
        for (index, payload) in payloads.into_iter().enumerate() {
            envelopes.push(haider_protocol::envelope::EventEnvelope {
                schema_version: haider_protocol::envelope::SCHEMA_VERSION,
                event_id: EventId::new(format!("effect-probe-{}-{}", answer.event_id, index + 1)),
                seq: 0,
                session_id: session_id.clone(),
                branch_id: answer.branch_id.clone(),
                run_id: answer.run_id.clone(),
                agent_id: answer.agent_id.clone(),
                device_id: self.inner.device_id.clone(),
                authority_epoch: answer.authority_epoch,
                worker_generation: self.inner.store.worker_generation(),
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
                        format!("effect probe payload could not serialize: {error}"),
                        false,
                    )
                })?,
            });
        }
        self.append(&mut envelopes).await?;
        Ok(())
    }

    /// Starts the real fresh turn selected by E6's `retry` handler. The
    /// original ambiguous effect is durably settled before this is called;
    /// the new turn receives an explicit probe-first instruction so it never
    /// blindly duplicates the prior mutation.
    async fn submit_effect_retry(
        &self,
        session_id: SessionId,
        effect: haider_protocol::ids::EffectId,
        menu_id: MenuId,
        resolution_seq: u64,
    ) -> Result<(), HaiderError> {
        let worker_generation = self.inner.store.worker_generation();
        let text = format!(
            "Retry unresolved effect {effect}. Probe the current state first; perform the operation only if it is still needed."
        );
        let command_id = format!("effect-retry-{menu_id}-{resolution_seq}");
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "effect": &effect,
            "menu": &menu_id,
            "resolution_seq": resolution_seq,
            "text": &text,
            "mode": DeliveryMode::Queue,
        }))
        .map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("effect retry coordinates could not serialize: {error}"),
                false,
            )
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let command = TurnAcceptCommand {
            command_id,
            request_digest,
            request_json,
            session_id: session_id.clone(),
            worker_generation,
            run_id: RunId::new(random_id("effect-retry-run").map_err(hub_error_as_store)?),
            agent_id: None,
            branch_id: None,
            text,
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new(
                random_id("effect-retry-queued").map_err(hub_error_as_store)?,
            ),
            user_event_id: EventId::new(
                random_id("effect-retry-user").map_err(hub_error_as_store)?,
            ),
            active_event_id: EventId::new(
                random_id("effect-retry-active").map_err(hub_error_as_store)?,
            ),
            device_id: self.inner.device_id.clone(),
        };
        let accepted = match self
            .accept_turn(command)
            .await
            .map_err(hub_error_as_store)?
        {
            TurnAcceptOutcome::Committed { accepted, .. }
            | TurnAcceptOutcome::IdempotentReplay { accepted } => accepted,
        };
        self.worker_manager()
            .map_err(hub_error_as_store)?
            .submit(accepted)
            .await
    }
}

async fn effect_probe_observation(
    intent: &EffectIntent,
    cwd: &str,
    delegation_count: Option<usize>,
) -> String {
    match &intent.class {
        EffectClass::FsRead | EffectClass::FsWrite => {
            let path = ["read ", "list ", "edit ", "write ", "patch "]
                .into_iter()
                .find_map(|prefix| intent.summary.strip_prefix(prefix));
            let Some(path) = path else {
                return "inconclusive — the durable file intent has no probeable path".into();
            };
            let path = std::path::PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                std::path::Path::new(cwd).join(path)
            };
            match tokio::task::spawn_blocking(move || std::fs::read(&path)).await {
                Ok(Ok(bytes)) => format!(
                    "re-read succeeded ({} bytes, blake3:{})",
                    bytes.len(),
                    blake3::hash(&bytes).to_hex()
                ),
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    "re-read found the target absent".into()
                }
                Ok(Err(error)) => format!("re-read failed: {error}"),
                Err(error) => format!("re-read task failed: {error}"),
            }
        }
        EffectClass::Network { .. } => {
            let Some(url) = intent.summary.strip_prefix("fetch ") else {
                return "inconclusive — this network effect has no safe idempotent probe".into();
            };
            let execution =
                haider_provider::fetch_public_url_with_one_retry(url, Some(8 * 1024)).await;
            match execution.outcome {
                Ok(outcome) => format!(
                    "GET probe succeeded after {} attempt(s): {} ({})",
                    execution.attempts, outcome.final_url, outcome.content_type
                ),
                Err(error) => format!(
                    "GET probe failed after {} attempt(s): {}",
                    execution.attempts, error.message
                ),
            }
        }
        EffectClass::AgentSpawn => format!(
            "durable delegation probe found {} child record(s)",
            delegation_count.unwrap_or(0)
        ),
        EffectClass::ProcessExec
        | EffectClass::GitOp
        | EffectClass::CredentialAccess
        | EffectClass::GuiAct => {
            "inconclusive — no safe automatic probe exists for this effect class".into()
        }
    }
}

impl HubConnection {
    async fn artifact_put(
        &self,
        request_id: RequestId,
        data_base64: String,
    ) -> Result<(), SessionHubError> {
        let decoded_len = match standard_base64_decoded_len(&data_base64) {
            Ok(decoded_len) => decoded_len,
            Err(message) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    message,
                    false,
                    None,
                );
            }
        };
        if decoded_len > ARTIFACT_PUT_MAX_BYTES {
            let actual_bytes = u64::try_from(decoded_len).unwrap_or(u64::MAX);
            return self.respond_error(
                request_id,
                ERROR_CODE_ARTIFACT_TOO_LARGE,
                &format!(
                    "artifact.put decodes to {actual_bytes} bytes; the hard limit is {ARTIFACT_PUT_MAX_BYTES}"
                ),
                false,
                Some(ErrorData::ArtifactTooLarge {
                    actual_bytes,
                    max_bytes: ARTIFACT_PUT_MAX_BYTES as u64,
                }),
            );
        }
        let bytes = match base64::engine::general_purpose::STANDARD.decode(data_base64) {
            Ok(bytes) => bytes,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &format!("artifact.put data_base64 is invalid: {error}"),
                    false,
                    None,
                );
            }
        };
        if bytes.len() > ARTIFACT_PUT_MAX_BYTES {
            let actual_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            return self.respond_error(
                request_id,
                ERROR_CODE_ARTIFACT_TOO_LARGE,
                &format!(
                    "artifact.put decoded {actual_bytes} bytes; the hard limit is {ARTIFACT_PUT_MAX_BYTES}"
                ),
                false,
                Some(ErrorData::ArtifactTooLarge {
                    actual_bytes,
                    max_bytes: ARTIFACT_PUT_MAX_BYTES as u64,
                }),
            );
        }
        let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let artifact = match self.hub.inner.store.put(bytes).await {
            Ok(artifact) => artifact,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ArtifactPut {
                artifact,
                bytes: byte_count,
            },
        })
    }

    /// Handles one request and enqueues its correlated response.
    pub async fn request(
        &self,
        request_id: RequestId,
        body: RequestBody,
    ) -> Result<(), SessionHubError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        match body {
            RequestBody::ArtifactPut { data_base64 } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.artifact_put(request_id, data_base64).await
            }
            RequestBody::SessionCreateWithPermissionOverrides {
                command_id,
                cwd,
                provider,
                model,
                max_tokens,
                permission_overrides,
                cache_policy,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_create(
                    request_id,
                    command_id,
                    cwd,
                    provider,
                    model,
                    max_tokens,
                    permission_overrides,
                    cache_policy.unwrap_or_default(),
                )
                .await
            }
            RequestBody::SessionCreate {
                command_id,
                cwd,
                provider,
                model,
                max_tokens,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_create(
                    request_id,
                    command_id,
                    cwd,
                    provider,
                    model,
                    max_tokens,
                    None,
                    Default::default(),
                )
                .await
            }
            RequestBody::SessionList { cursor, limit } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_list(request_id, cursor, limit).await
            }
            RequestBody::SessionRead { session_id, range } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_read(request_id, session_id, range).await
            }
            RequestBody::SessionObserve {
                session_id,
                last_event_limit,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_observe(request_id, session_id, last_event_limit)
                    .await
            }
            RequestBody::SessionFleet { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_fleet(request_id, session_id).await
            }
            RequestBody::GraphStatus { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_status(request_id, session_id).await
            }
            RequestBody::GraphInspect {
                session_id,
                cursor,
                limit,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_inspect(request_id, session_id, cursor, limit)
                    .await
            }
            RequestBody::SessionDiagnostic {
                command_id,
                session_id,
                code,
                message,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "session diagnostic requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_diagnostic(request_id, command_id, session_id, code, message)
                    .await
            }
            RequestBody::HooksList { cwd } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.hooks_list(request_id, cwd).await
            }
            RequestBody::HooksTrust { command_id, digest } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.hooks_trust(request_id, command_id, digest, true).await
            }
            RequestBody::HooksRevoke { command_id, digest } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.hooks_trust(request_id, command_id, digest, false)
                    .await
            }
            RequestBody::SessionAttach {
                session_id,
                after_seq,
                mode,
            } => {
                let operation = match mode {
                    AttachMode::View => Operation::View,
                    AttachMode::Control => Operation::Control,
                    // `Unknown` and any future mode: never guess an
                    // authorization level for a mode this daemon predates.
                    _ => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "unknown attachment mode",
                            false,
                            None,
                        );
                    }
                };
                if let Err(message) = authorize(&self.capabilities, operation) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_attach(request_id, session_id, after_seq, mode)
                    .await
            }
            RequestBody::SessionDetach { attachment_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_detach(request_id, attachment_id).await
            }
            RequestBody::BranchCreate {
                command_id,
                session_id,
                worker_generation,
                source_branch_id,
                fork_node_id,
                fork_seq,
                name,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.branch_create(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    source_branch_id,
                    fork_node_id,
                    fork_seq,
                    name,
                )
                .await
            }
            RequestBody::AgentMessage {
                command_id,
                session_id,
                worker_generation,
                agent,
                text,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "agent messaging requires a control attachment to the parent session",
                        false,
                        None,
                    );
                }
                self.agent_message(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    agent,
                    text,
                )
                .await
            }
            RequestBody::TurnSubmitWithBranch {
                command_id,
                session_id,
                worker_generation,
                branch_id,
                text,
                attachments,
                mode,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn submission requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    branch_id,
                    text,
                    attachments,
                    mode,
                    false,
                )
                .await
            }
            RequestBody::TurnSubmit {
                command_id,
                session_id,
                worker_generation,
                text,
                attachments,
                mode,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn submission requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    None,
                    text,
                    attachments,
                    mode,
                    false,
                )
                .await
            }
            RequestBody::TurnSubmitWithHookTrust {
                command_id,
                session_id,
                worker_generation,
                branch_id,
                text,
                attachments,
                mode,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn submission requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    branch_id,
                    text,
                    attachments,
                    mode,
                    true,
                )
                .await
            }
            RequestBody::TurnCancel {
                command_id,
                session_id,
                worker_generation,
                run_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn cancellation requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_cancel(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    run_id,
                )
                .await
            }
            RequestBody::SessionCompactOnBranch {
                command_id,
                session_id,
                worker_generation,
                branch_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "context compaction requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_compact(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    branch_id,
                )
                .await
            }
            RequestBody::SessionCompact {
                command_id,
                session_id,
                worker_generation,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "context compaction requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_compact(request_id, command_id, session_id, worker_generation, None)
                    .await
            }
            RequestBody::SessionSelectModel {
                command_id,
                session_id,
                worker_generation,
                model,
                provider,
                confirm_new_epoch,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "model selection requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_select_model(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    model,
                    provider,
                    confirm_new_epoch,
                )
                .await
            }
            RequestBody::SessionRename {
                command_id,
                session_id,
                worker_generation,
                title,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "session rename requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_rename(request_id, command_id, session_id, worker_generation, title)
                    .await
            }
            RequestBody::GraphPin {
                command_id,
                session_id,
                worker_generation,
                template,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_pin(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    template,
                )
                .await
            }
            RequestBody::GraphRunSetOpen {
                command_id,
                session_id,
                worker_generation,
                plan_item_id,
                plan_event_seq,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_run_set_open(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    plan_item_id,
                    plan_event_seq,
                )
                .await
            }
            RequestBody::GraphSwitch {
                command_id,
                session_id,
                worker_generation,
                old_graph_id,
                template,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_switch(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    old_graph_id,
                    template,
                )
                .await
            }
            RequestBody::GraphAbandon {
                command_id,
                session_id,
                worker_generation,
                why,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_abandon(request_id, command_id, session_id, worker_generation, why)
                    .await
            }
            RequestBody::SessionSelectEffort {
                command_id,
                session_id,
                worker_generation,
                effort,
                confirm_new_epoch,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "effort selection requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_select_effort(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    effort,
                    confirm_new_epoch,
                )
                .await
            }
            RequestBody::SessionSelectFast {
                command_id,
                session_id,
                worker_generation,
                enabled,
                confirm_new_epoch,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "fast-mode selection requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_select_fast(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    enabled,
                    confirm_new_epoch,
                )
                .await
            }
            RequestBody::ShellExec {
                command_id,
                session_id,
                worker_generation,
                command,
                cwd,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "direct shell execution requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.shell_exec(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    command,
                    cwd,
                )
                .await
            }
            RequestBody::ToolsInventory { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.tools_inventory(request_id, session_id).await
            }
            RequestBody::VaultStage {
                stage_id,
                purpose,
                secret,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.vault_stage(request_id, stage_id, purpose, secret)
            }
            RequestBody::TranscriptionSecretGet => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.transcription_secret_get(request_id)
            }
            RequestBody::TranscriptionSecretSet { secret, clear } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.transcription_secret_set(request_id, secret, clear)
            }
            RequestBody::AccountLoginApi {
                command_id,
                provider,
                alias,
                vault_reference,
                validation_model,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_login(
                    request_id,
                    command_id,
                    provider,
                    alias,
                    vault_reference,
                    validation_model,
                )
            }
            RequestBody::AccountOAuthStart {
                provider,
                desired_alias,
                attempt_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_start(request_id, provider, desired_alias, attempt_id)
            }
            RequestBody::AccountOAuthStatus {
                flow_id,
                attempt_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_status(request_id, flow_id, attempt_id)
            }
            RequestBody::AccountOAuthCancel {
                flow_id,
                attempt_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_cancel(request_id, flow_id, attempt_id)
            }
            RequestBody::AccountOAuthImport { command_id, source } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_import(request_id, command_id, source)
            }
            RequestBody::AccountDeviceCandidates => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_device_candidates(request_id)
            }
            RequestBody::AccountImportDevice {
                command_id,
                candidate,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_import_device(request_id, command_id, candidate)
            }
            RequestBody::AccountAdd {
                command_id,
                provider,
                alias,
                auth_method,
                flow_id,
                attempt_id,
                oauth_reference,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_add_oauth(
                    request_id,
                    command_id,
                    provider,
                    alias,
                    auth_method,
                    flow_id,
                    attempt_id,
                    oauth_reference,
                )
            }
            RequestBody::AccountSetActive {
                command_id,
                alias,
                confirm_new_epoch,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_set_active(request_id, command_id, alias, confirm_new_epoch)
                    .await
            }
            RequestBody::AccountRemove {
                command_id,
                alias,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_remove(request_id, command_id, alias, expected_revision)
            }
            RequestBody::AccountSetDefaultModel {
                command_id,
                provider,
                model,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_set_default_model(
                    request_id,
                    command_id,
                    provider,
                    model,
                    expected_revision,
                )
            }
            RequestBody::AccountList { provider } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_list(request_id, provider)
            }
            RequestBody::ProviderList { provider } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.provider_list(request_id, provider)
            }
            RequestBody::ProviderModelsRefresh { provider } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.provider_models_refresh(request_id, provider)
            }
            RequestBody::ProviderConfigure {
                command_id,
                provider,
                api_family,
                origin,
                auth_requirement,
                enabled,
                models,
                default_model,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.provider_configure(
                    request_id,
                    command_id,
                    crate::provider_registry::ProviderConfigureInput {
                        provider,
                        api_family,
                        origin,
                        auth_requirement,
                        enabled,
                        models,
                        default_model,
                    },
                    expected_revision,
                )
            }
            RequestBody::ProviderRemove {
                command_id,
                provider,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.provider_remove(request_id, command_id, provider, expected_revision)
            }
            RequestBody::UsageReport => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.usage_report(request_id).await
            }
            // `Unknown` and any future method decode alike: a typed,
            // correlated rejection instead of a dropped request.
            _ => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unknown session method",
                false,
                None,
            ),
        }
    }

    /// The transport + vault gate shared by `vault.stage` and
    /// `account.login_api` (R7/R10): Control alone must not expose raw-secret
    /// staging to a remote transport, and a vaultless platform answers the
    /// stable `vault_unsupported` BEFORE staging/validation.
    fn secret_surface_facade(
        &self,
        request_id: &RequestId,
    ) -> Result<Option<crate::accounts::AccountsFacade>, SessionHubError> {
        if self.transport != crate::accounts::ConnectionTransport::LocalSameUid {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_CAPABILITY_DENIED,
                "secret staging is only served on authenticated same-UID local connections",
                false,
                None,
            )?;
            return Ok(None);
        }
        let facade = self.hub.accounts()?;
        match facade {
            Some(facade) if facade.vault_supported => Ok(Some(facade)),
            _ => {
                self.respond_error(
                    request_id.clone(),
                    haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                    "this platform has no supported secret vault (W3c supports macOS Keychain)",
                    false,
                    None,
                )?;
                Ok(None)
            }
        }
    }

    fn device_surface_facade(
        &self,
        request_id: &RequestId,
    ) -> Result<Option<crate::accounts::AccountsFacade>, SessionHubError> {
        if self.transport != crate::accounts::ConnectionTransport::LocalSameUid {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_CAPABILITY_DENIED,
                "device credential discovery is only served on authenticated same-UID local connections",
                false,
                None,
            )?;
            return Ok(None);
        }
        let Some(facade) = self.hub.accounts()? else {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_DRAINING,
                "account actor is unavailable",
                true,
                None,
            )?;
            return Ok(None);
        };
        Ok(Some(facade))
    }

    /// `vault.stage`: connection-scoped, non-durable, inline (no I/O). The
    /// secret enters zeroizing storage here and the wire frame drops
    /// (zeroized) with this call.
    fn vault_stage(
        &self,
        request_id: RequestId,
        stage_id: String,
        purpose: haider_rpc::StagePurpose,
        secret: haider_rpc::SecretWire,
    ) -> Result<(), SessionHubError> {
        if self.secret_surface_facade(&request_id)?.is_none() {
            return Ok(());
        }
        if stage_id.trim().is_empty() || secret.is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "stage id and secret must not be empty",
                false,
                None,
            );
        }
        if matches!(purpose, haider_rpc::StagePurpose::Unknown) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unknown stage purpose",
                false,
                None,
            );
        }
        let staged = {
            let mut stages = lock(&self.stages)?;
            stages.stage(&stage_id, purpose, secret.expose_secret().as_bytes())
        };
        match staged {
            Ok((vault_reference, expires_at_ms)) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::VaultStage {
                    stage_id,
                    vault_reference,
                    expires_at_ms,
                },
            }),
            Err(crate::accounts::StageError::Mismatch) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "stage id was already used with different secret bytes",
                false,
                None,
            ),
            Err(crate::accounts::StageError::Mint(message)) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &format!("cannot mint stage reference: {message}"),
                true,
                None,
            ),
        }
    }

    /// `transcription.secret_get` (T1): answers the vaulted Deepgram key on
    /// the same-UID local UDS surface only. Inline like `vault.stage`: one
    /// bounded ≤512-byte vault file read, comparable to one store
    /// transaction. A missing entry is an honest `secret: None`, never an
    /// error — "no key yet" is a first-class setup state.
    fn transcription_secret_get(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let Some(vault) = facade.vault else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault",
                false,
                None,
            );
        };
        let alias = transcription_secret_alias();
        match vault.resolve(&alias) {
            Ok(secret) => {
                let Ok(value) = std::str::from_utf8(secret.expose_secret()) else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "stored transcription secret is not valid UTF-8; set it again",
                        false,
                        None,
                    );
                };
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::TranscriptionSecretGet {
                        secret: Some(haider_rpc::SecretWire::new(value)),
                    },
                })
            }
            Err(error) if error.code == haider_protocol::error::ErrorCode::CredentialMissing => {
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::TranscriptionSecretGet { secret: None },
                })
            }
            // The vault's own message carries an alias at most, never
            // secret bytes (haider-accounts redaction law).
            Err(error) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &format!("could not read transcription secret: {}", error.message),
                error.retryable,
                None,
            ),
        }
    }

    /// `transcription.secret_set` (T1): stores or clears the Deepgram key
    /// in the profile vault. ADE key hygiene is enforced BEFORE any vault
    /// write: non-empty, ≤512 chars, no control bytes — and no refusal ever
    /// echoes key material.
    fn transcription_secret_set(
        &self,
        request_id: RequestId,
        secret: haider_rpc::SecretWire,
        clear: bool,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let Some(vault) = facade.vault else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault",
                false,
                None,
            );
        };
        let alias = transcription_secret_alias();
        if clear {
            if !secret.is_empty() {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "clear:true must not carry a secret",
                    false,
                    None,
                );
            }
            return match vault.delete(&alias) {
                Ok(()) => self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::TranscriptionSecretSet { present: false },
                }),
                Err(error) => self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &format!("could not clear transcription secret: {}", error.message),
                    error.retryable,
                    None,
                ),
            };
        }
        let value = secret.expose_secret().trim();
        if value.is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "transcription secret must not be empty",
                false,
                None,
            );
        }
        if value.len() > TRANSCRIPTION_SECRET_MAX_LEN || value.chars().any(char::is_control) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "transcription secret is not a valid API key",
                false,
                None,
            );
        }
        match vault.put(&alias, value.as_bytes()) {
            Ok(()) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::TranscriptionSecretSet { present: true },
            }),
            Err(error) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &format!("could not store transcription secret: {}", error.message),
                error.retryable,
                None,
            ),
        }
    }

    /// `account.login_api`: claims the stage and HANDS OFF to the account
    /// actor (R7: the connection task never awaits validation/Keychain work
    /// inline). The correlated response arrives from the actor through this
    /// connection's sink; disconnect drops only that route, never the
    /// durable command.
    fn account_login(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        alias: Option<String>,
        vault_reference: String,
        validation_model: Option<String>,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || vault_reference.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "login command id, provider, and vault reference must not be empty",
                false,
                None,
            );
        }
        let claimed = {
            let mut stages = lock(&self.stages)?;
            stages.claim(&vault_reference)
        };
        let secret = match claimed {
            Some((haider_rpc::StagePurpose::ApiKey, secret)) => Some(secret),
            Some(_) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "staged secret was not staged for api_key use",
                    false,
                    None,
                );
            }
            // Unknown/expired reference: the actor may still hold the
            // pending command's secret (retry-after-retryable), else it
            // answers restage_required.
            None => None,
        };
        let Some(login) = facade.login else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault (W3c supports macOS Keychain)",
                false,
                None,
            );
        };
        let job = crate::accounts::LoginJob {
            command_id: command_id.0,
            provider,
            display_alias: alias.filter(|value| !value.trim().is_empty()),
            validation_model: validation_model.filter(|value| !value.trim().is_empty()),
            secret,
            route: crate::accounts::LoginRoute {
                request_id: request_id.clone(),
                sink: Arc::clone(&self.sink),
            },
        };
        match login.try_send(crate::accounts::AccountCommand::Login(Box::new(job))) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                // Honest recovery: the single-use stage was already claimed
                // and dropped with this rejected job, so the retry needs a
                // fresh stage (the restage protocol covers it).
                "account actor is busy; stage the key again and retry",
                true,
                None,
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            ),
        }
    }

    fn account_oauth_start(
        &self,
        request_id: RequestId,
        provider: String,
        desired_alias: String,
        attempt_id: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let Some(oauth) = facade.oauth else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_OAUTH_UNAVAILABLE,
                "OAuth coordinator is unavailable",
                false,
                None,
            );
        };
        if provider.trim().is_empty()
            || desired_alias.trim().is_empty()
            || attempt_id.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "OAuth provider, alias, and attempt id must not be empty",
                false,
                None,
            );
        }
        let availability = oauth.availability(&provider, true);
        if !availability.available {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountOAuthStart {
                    availability,
                    flow_id: None,
                    authorization_url: None,
                    provider_origin: None,
                    loopback_port: None,
                    expires_at_ms: None,
                },
            });
        }
        let route = crate::oauth::OAuthRoute {
            request_id: request_id.clone(),
            sink: Arc::clone(&self.sink),
        };
        match oauth.try_start(
            &self.connection_id,
            provider,
            desired_alias,
            attempt_id,
            route,
        ) {
            Ok(()) => Ok(()),
            Err(crate::oauth::StartAdmissionError::Busy) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                "OAuth coordinator is busy",
                true,
                None,
            ),
            Err(crate::oauth::StartAdmissionError::Closed) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "OAuth coordinator is shut down",
                true,
                None,
            ),
        }
    }

    fn account_oauth_import(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        source: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty()
            || !matches!(source.as_str(), "codex" | "claude-code" | "kimi-code")
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.oauth_import requires a command id and source `codex`, `claude-code`, or `kimi-code`",
                false,
                None,
            );
        }
        let Some(commands) = facade.login else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            );
        };
        let job = crate::accounts::OAuthImportJob {
            command_id: command_id.0,
            source,
            route: crate::accounts::LoginRoute {
                request_id: request_id.clone(),
                sink: Arc::clone(&self.sink),
            },
        };
        match commands.try_send(crate::accounts::AccountCommand::ImportOAuth(Box::new(job))) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                "account actor is busy; retry with the same command id",
                true,
                None,
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            ),
        }
    }

    fn account_device_candidates(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(facade) = self.device_surface_facade(&request_id)? else {
            return Ok(());
        };
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::DeviceCandidates {
                discovery_disabled: facade.discovery_disabled,
                completed: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            },
        )
    }

    fn account_import_device(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        candidate: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty() || !valid_device_candidate_id(&candidate) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.import_device requires a command id and valid opaque candidate id",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::ImportDevice(Box::new(
                crate::accounts::DeviceImportJob {
                    command_id: command_id.0,
                    candidate,
                    discovery_disabled: facade.discovery_disabled,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn account_oauth_status(
        &self,
        request_id: RequestId,
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let status = facade
            .oauth
            .and_then(|oauth| oauth.status(&self.connection_id, &flow_id, &attempt_id));
        match status {
            Some(status) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountOAuthStatus { flow_id, status },
            }),
            None => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_OAUTH_FLOW_NOT_FOUND,
                "OAuth flow is unavailable for this connection and attempt",
                true,
                None,
            ),
        }
    }

    fn account_oauth_cancel(
        &self,
        request_id: RequestId,
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let status = facade
            .oauth
            .and_then(|oauth| oauth.cancel(&self.connection_id, &flow_id, &attempt_id));
        match status {
            Some(status) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountOAuthCancel { flow_id, status },
            }),
            None => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_OAUTH_FLOW_NOT_FOUND,
                "OAuth flow is unavailable for this connection and attempt",
                true,
                None,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn account_add_oauth(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        alias: String,
        auth_method: haider_rpc::AccountAddMethod,
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
        oauth_reference: haider_rpc::OAuthReadyRefWire,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if !matches!(auth_method, haider_rpc::AccountAddMethod::OAuth)
            || command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || alias.trim().is_empty()
            || attempt_id.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.add requires OAuth method and complete coordinates",
                false,
                None,
            );
        }
        let Some(oauth) = facade.oauth else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_OAUTH_UNAVAILABLE,
                "OAuth coordinator is unavailable",
                false,
                None,
            );
        };
        let claim = oauth.claim_ready(
            &self.connection_id,
            &flow_id,
            &attempt_id,
            &provider,
            &alias,
            &oauth_reference,
        );
        let Some(login) = facade.login else {
            if let Some(claim) = claim {
                oauth.restore_ready(claim);
            }
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault",
                false,
                None,
            );
        };
        let job = crate::accounts::OAuthAddJob {
            command_id: command_id.0,
            provider,
            display_alias: alias,
            claim,
            route: crate::accounts::LoginRoute {
                request_id: request_id.clone(),
                sink: Arc::clone(&self.sink),
            },
        };
        match login.try_send(crate::accounts::AccountCommand::AddOAuth(Box::new(job))) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(command)) => {
                if let crate::accounts::AccountCommand::AddOAuth(job) = command
                    && let Some(claim) = job.claim
                {
                    oauth.restore_ready(claim);
                }
                self.respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_BUSY,
                    "account actor is busy; retry with the same OAuth reference",
                    true,
                    None,
                )
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            ),
        }
    }

    fn send_management_command(
        &self,
        request_id: RequestId,
        command: crate::accounts::AccountCommand,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.hub.accounts()? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account/provider actor is unavailable",
                true,
                None,
            );
        };
        let Some(commands) = facade.login else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account/provider actor is unavailable",
                true,
                None,
            );
        };
        match commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                "account/provider actor mailbox is full",
                true,
                None,
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account/provider actor is shut down",
                true,
                None,
            ),
        }
    }

    async fn account_set_active(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        alias: String,
        confirm_new_epoch: bool,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || alias.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "set-active command id and alias must not be empty",
                false,
                None,
            );
        }
        let descriptor = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read())
            .and_then(|view| {
                view.descriptors
                    .into_iter()
                    .find(|descriptor| descriptor.alias.as_str() == alias)
            });
        if let Some(descriptor) = descriptor {
            let target_auth_scope = match descriptor.auth_method {
                haider_protocol::credential::AuthMethod::ApiKey => "api_key",
                haider_protocol::credential::AuthMethod::OAuth => "oauth_subscription",
            };
            let mut warnings = Vec::new();
            for session_id in self.hub.inner.store.session_ids().await? {
                let Some(metadata) = self.hub.inner.store.session_metadata(&session_id).await?
                else {
                    continue;
                };
                if metadata.provider != descriptor.provider {
                    continue;
                }
                let scope = crate::cache_policy::latest_main_cache_scope(
                    &self.hub.inner.store,
                    &session_id,
                )
                .await?;
                let Some(scope) = scope else {
                    continue;
                };
                let mut changed_fields = Vec::new();
                if scope.account_scope.as_ref().map(|value| value.as_str())
                    != Some(descriptor.alias.as_str())
                {
                    changed_fields.push("account".to_owned());
                }
                if scope.auth_scope != target_auth_scope {
                    changed_fields.push("auth".to_owned());
                }
                if let Some(warning) = crate::cache_policy::assess_cache_change(
                    &metadata,
                    Some(&scope),
                    &metadata.provider,
                    &metadata.model,
                    Some(target_auth_scope),
                    changed_fields,
                    false,
                ) {
                    warnings.push(warning);
                }
            }
            if let Some(warning) = crate::cache_policy::combine_cache_change_warnings(warnings)
                && crate::cache_policy::blocks_change(&warning, confirm_new_epoch)
            {
                return self.respond_cache_confirmation_required(request_id, &warning);
            }
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SetActive(Box::new(crate::accounts::SetActiveJob {
                command_id: command_id.0,
                alias,
                route: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            })),
        )
    }

    fn account_remove(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        alias: String,
        expected_revision: Option<u64>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || alias.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "remove command id and alias must not be empty",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::Remove(Box::new(crate::accounts::RemoveAccountJob {
                command_id: command_id.0,
                alias,
                expected_revision,
                route: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            })),
        )
    }

    fn account_set_default_model(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        model: String,
        expected_revision: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || model.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "default-model command id, provider, and model must not be empty",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SetDefaultModel(Box::new(
                crate::accounts::SetDefaultModelJob {
                    command_id: command_id.0,
                    provider,
                    model,
                    expected_revision,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn provider_configure(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        input: crate::provider_registry::ProviderConfigureInput,
        expected_revision: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || input.provider.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "provider-configure command id and provider must not be empty",
                false,
                None,
            );
        }
        if input
            .api_family
            .is_some_and(|family| matches!(family, haider_rpc::ProviderApiFamilyWire::Unknown))
            || input.auth_requirement.is_some_and(|requirement| {
                matches!(
                    requirement,
                    haider_rpc::ProviderAuthRequirementWire::Unknown
                )
            })
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "provider configuration contains an unknown identity field",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::ConfigureProvider(Box::new(
                crate::accounts::ProviderConfigureJob {
                    command_id: command_id.0,
                    input,
                    expected_revision,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn provider_remove(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        expected_revision: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || provider.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "provider-remove command id and provider must not be empty",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::RemoveProvider(Box::new(
                crate::accounts::ProviderRemoveJob {
                    command_id: command_id.0,
                    provider,
                    expected_revision,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn provider_models_refresh(
        &self,
        request_id: RequestId,
        provider: String,
    ) -> Result<(), SessionHubError> {
        if provider.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "model-refresh provider must not be empty",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::RefreshProviderModels {
                provider,
                completed: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            },
        )
    }

    /// `account.list`: inline snapshot read (short command; the actor is the
    /// only writer, so a queued login never head-of-line-blocks listing).
    fn account_list(
        &self,
        request_id: RequestId,
        provider: Option<String>,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.hub.accounts()? else {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountList {
                    descriptors: Vec::new(),
                    revision: None,
                    provider_active: Vec::new(),
                    provider_defaults: Vec::new(),
                },
            });
        };
        let Some(view) = facade.management.read() else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "management snapshot is unavailable",
                true,
                None,
            );
        };
        let descriptors = view
            .descriptors
            .iter()
            .filter(|descriptor| {
                provider
                    .as_deref()
                    .is_none_or(|provider| descriptor.provider == provider)
            })
            .cloned()
            .collect::<Vec<_>>();
        let provider_active = view
            .descriptors
            .iter()
            .filter(|descriptor| {
                descriptor.active
                    && provider
                        .as_deref()
                        .is_none_or(|provider| descriptor.provider == provider)
            })
            .map(|descriptor| haider_rpc::ProviderActiveWire {
                provider: descriptor.provider.clone(),
                alias: descriptor.alias.clone(),
            })
            .collect();
        let provider_defaults = view
            .providers
            .iter()
            .filter(|summary| {
                provider
                    .as_deref()
                    .is_none_or(|provider| summary.provider == provider)
            })
            .filter_map(|summary| {
                summary
                    .default_model
                    .as_ref()
                    .map(|model| haider_rpc::ProviderDefaultWire {
                        provider: summary.provider.clone(),
                        model: model.clone(),
                    })
            })
            .collect();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::AccountList {
                descriptors,
                revision: Some(view.revision),
                provider_active,
                provider_defaults,
            },
        })
    }

    /// `provider.list`: a short, cached management-snapshot read. Endpoint
    /// probing and provider validation are never performed on the connection
    /// task.
    fn provider_list(
        &self,
        request_id: RequestId,
        provider: Option<String>,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.hub.accounts()? else {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::ProviderList {
                    providers: Vec::new(),
                    revision: 0,
                },
            });
        };
        let Some(view) = facade.management.read() else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "management snapshot is unavailable",
                true,
                None,
            );
        };
        let providers = filter_provider_summaries(view.providers, provider.as_deref());
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ProviderList {
                providers,
                revision: view.revision,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn branch_create(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        source_branch_id: Option<haider_protocol::ids::BranchId>,
        fork_node_id: haider_protocol::ids::NodeId,
        fork_seq: u64,
        name: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty()
            || fork_node_id.as_str().is_empty()
            || fork_seq == 0
            || name.as_ref().is_some_and(|name| name.trim().is_empty())
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "branch command, fork node/sequence, and optional name must be valid",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "source_branch_id": &source_branch_id,
            "fork_node_id": &fork_node_id,
            "fork_seq": fork_seq,
            "name": &name,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode branch-create coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt replay precedes attachment, generation, and current-lineage
        // validation so a lost response remains recoverable after restart.
        match self
            .hub
            .branch_create_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(created)) => return self.respond_branch_created(request_id, created),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "branch creation requires a control attachment to this session",
                false,
                None,
            );
        }

        let command = BranchCreateCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            branch_id: haider_protocol::ids::BranchId::new(random_id("branch")?),
            source_branch_id,
            fork_node_id,
            fork_seq,
            name,
            event_id: EventId::new(random_id("branch-created")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let created = match self.hub.create_branch(command).await {
            Ok(BranchCreateOutcome::Committed { created, .. })
            | Ok(BranchCreateOutcome::IdempotentReplay { created }) => created,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_branch_created(request_id, created)
    }

    fn respond_branch_created(
        &self,
        request_id: RequestId,
        created: CreatedBranch,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::BranchCreate {
                session_id: created.session_id,
                branch_id: created.branch_id,
                source_branch_id: created.source_branch_id,
                fork_node_id: created.fork_node_id,
                fork_seq: created.fork_seq,
                created_seq: created.created_seq,
                worker_generation: created.worker_generation,
                name: created.name,
            },
        })
    }

    /// `session.select_model` — receipted live-session model selection.
    ///
    /// Sessions are provider-agnostic: this is exactly as ceremonial as
    /// picking a model. Resolution/validation ride the ONE authority in
    /// `crate::model_select`; the store owns durability; the next logical
    /// turn re-reads the committed metadata (R6 re-resolution), so commit
    /// here IS next-turn pickup.
    #[allow(clippy::too_many_arguments)]
    async fn session_select_model(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        model: String,
        provider: Option<String>,
        confirm_new_epoch: bool,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || model.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "model selection needs a command id and a model",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "model": &model,
            "provider": &provider,
            "confirm_new_epoch": confirm_new_epoch,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!(
                "cannot encode model-selection coordinates: {error}"
            ))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt replay precedes validation so a lost response remains
        // recoverable even after registry or inventory changes.
        match self
            .hub
            .session_select_model_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(selected)) => return self.respond_model_selected(request_id, selected),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        let Some(current) = (match self.hub.session_metadata(&session_id).await {
            Ok(metadata) => metadata,
            Err(error) => return self.respond_turn_error(request_id, error),
        }) else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "model selection requires a live session with typed metadata",
                false,
                None,
            );
        };
        let (summaries, descriptors) = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read())
            .map_or_else(
                || (Vec::new(), Vec::new()),
                |view| (view.providers, view.descriptors),
            );
        let authority = crate::model_select::ModelSelectionAuthority::new(
            self.hub.creatable_providers()?,
            summaries,
        );
        let (resolved_provider, resolved_model) =
            match authority.validate_selection(&current.provider, provider.as_deref(), &model) {
                Ok(pair) => pair,
                Err(refusal) => return self.respond_selection_refusal(request_id, &refusal),
            };
        let target_descriptor = descriptors
            .iter()
            .find(|descriptor| descriptor.provider == resolved_provider && descriptor.active)
            .cloned();
        let mut changed_fields = Vec::new();
        if current.provider != resolved_provider {
            changed_fields.push("provider".to_owned());
        }
        if current.model != resolved_model {
            changed_fields.push("model".to_owned());
        }
        let current_scope =
            crate::cache_policy::latest_main_cache_scope(&self.hub.inner.store, &session_id)
                .await?;
        let target_auth_scope =
            target_descriptor
                .as_ref()
                .map(|descriptor| match descriptor.auth_method {
                    haider_protocol::credential::AuthMethod::ApiKey => "api_key",
                    haider_protocol::credential::AuthMethod::OAuth => "oauth_subscription",
                });
        if let Some(scope) = current_scope.as_ref() {
            if let Some(target_auth_scope) = target_auth_scope
                && scope.auth_scope != target_auth_scope
            {
                changed_fields.push("auth".to_owned());
            }
            let target_account = target_descriptor
                .as_ref()
                .map(|descriptor| &descriptor.alias);
            if target_descriptor.is_some() && scope.account_scope.as_ref() != target_account {
                changed_fields.push("account".to_owned());
            }
        }
        if let Some(warning) = crate::cache_policy::assess_cache_change(
            &current,
            current_scope.as_ref(),
            &resolved_provider,
            &resolved_model,
            target_auth_scope,
            changed_fields,
            false,
        ) && crate::cache_policy::blocks_change(&warning, confirm_new_epoch)
        {
            return self.respond_cache_confirmation_required(request_id, &warning);
        }

        let command = SessionSelectModelCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            provider: resolved_provider,
            model: resolved_model,
            event_id: EventId::new(random_id("model-selected")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.select_session_model(command).await {
            Ok(SessionSelectModelOutcome::Committed { selected, .. })
            | Ok(SessionSelectModelOutcome::IdempotentReplay { selected }) => selected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_model_selected(request_id, selected)
    }

    fn respond_selection_refusal(
        &self,
        request_id: RequestId,
        refusal: &crate::model_select::SelectionRefusal,
    ) -> Result<(), SessionHubError> {
        use crate::model_select::SelectionRefusal;
        let (code, data) = match refusal {
            SelectionRefusal::ProviderUnavailable { provider } => (
                haider_rpc::ERROR_CODE_PROVIDER_UNAVAILABLE,
                Some(ErrorData::ProviderUnavailable {
                    provider: provider.clone(),
                }),
            ),
            SelectionRefusal::ModelUnknown { provider, model } => (
                haider_rpc::ERROR_CODE_MODEL_UNKNOWN,
                Some(ErrorData::ModelUnknown {
                    provider: provider.clone(),
                    model: model.clone(),
                }),
            ),
            SelectionRefusal::ModelNotResolvable { .. }
            | SelectionRefusal::InvalidSelector { .. } => (ERROR_CODE_INVALID_ARGUMENT, None),
        };
        self.respond_error(request_id, code, &refusal.message(), false, data)
    }

    fn respond_model_selected(
        &self,
        request_id: RequestId,
        selected: SelectedModel,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSelectModel {
                session_id: selected.session_id,
                provider: selected.provider,
                model: selected.model,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }

    /// `session.rename` (G2) — receipted live-session rename, the exact
    /// `session.select_model` shape: normalization here, durability in the
    /// store's one transaction, receipt replay BEFORE validation so a lost
    /// response stays recoverable, and the same worker-generation fence.
    async fn session_rename(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        title: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session rename needs a command id",
                false,
                None,
            );
        }
        let title = normalize_session_title(title);
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "title": &title,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode session-rename coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt replay precedes validation so a lost response remains
        // recoverable even after metadata changes.
        match self
            .hub
            .session_rename_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(renamed)) => return self.respond_session_renamed(request_id, renamed),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        let command = SessionRenameCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            title,
            only_if_untitled: false,
            event_id: EventId::new(random_id("session-renamed")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let renamed = match self.hub.rename_session(command).await {
            Ok(SessionRenameOutcome::Committed { renamed, .. })
            | Ok(SessionRenameOutcome::IdempotentReplay { renamed }) => renamed,
            Ok(SessionRenameOutcome::Skipped) => {
                // The guard is auto-title-only; an explicit rename never
                // sets it, so this arm is unreachable by construction.
                return Err(SessionHubError::Task(
                    "explicit session rename cannot be skipped".into(),
                ));
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_session_renamed(request_id, renamed)
    }

    fn respond_session_renamed(
        &self,
        request_id: RequestId,
        renamed: RenamedSession,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionRename {
                session_id: renamed.session_id,
                title: renamed.title,
                renamed_seq: renamed.renamed_seq,
                worker_generation: renamed.worker_generation,
            },
        })
    }

    async fn graph_status(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        let status = match self.hub.graph_status(&session_id).await {
            Ok(status) => status,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphStatus { status },
        })
    }

    async fn graph_inspect(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<(), SessionHubError> {
        let inspected = match self.hub.graph_inspect(&session_id, cursor, limit).await {
            Ok(inspected) => inspected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphInspect {
                snapshot: inspected.snapshot,
                next_cursor: inspected.next_cursor,
            },
        })
    }

    async fn graph_pin(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        template: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "graph pin needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "template": &template,
        }))
        .map_err(|error| SessionHubError::Task(format!("cannot encode graph pin: {error}")))?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .graph_pin_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(pinned)) => return self.respond_graph_pinned(request_id, pinned),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        // Lost-response replay above is deliberately unfenced. A genuinely
        // new mutation still requires this connection's live control lease.
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "graph pin requires a control attachment to this session",
                false,
                None,
            );
        }
        let command = GraphPinCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            graph_id: GraphId::new(random_id("graph")?),
            template,
            device_id: self.hub.inner.device_id.clone(),
        };
        let pinned = match self.hub.pin_graph(command).await {
            Ok(GraphPinOutcome::Committed { pinned, .. })
            | Ok(GraphPinOutcome::IdempotentReplay { pinned }) => pinned,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_graph_pinned(request_id, pinned)
    }

    fn respond_graph_pinned(
        &self,
        request_id: RequestId,
        pinned: PinnedGraph,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphPin {
                session_id: pinned.session_id,
                graph_id: pinned.graph_id,
                template: pinned.template,
                digest: pinned.digest,
                pinned_seq: pinned.pinned_seq,
                opened_seq: pinned.opened_seq,
                worker_generation: pinned.worker_generation,
            },
        })
    }

    async fn graph_run_set_open(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        plan_item_id: ItemId,
        plan_event_seq: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || plan_event_seq == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "graph run-set open needs a command id and nonzero Plan event sequence",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "plan_item_id": &plan_item_id,
            "plan_event_seq": plan_event_seq,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode graph run-set open: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .graph_run_set_open_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(opened)) => return self.respond_graph_run_set_opened(request_id, opened),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "graph run-set open requires a control attachment to this session",
                false,
                None,
            );
        }
        let command = GraphRunSetOpenCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            plan_item_id,
            plan_event_seq,
            device_id: self.hub.inner.device_id.clone(),
        };
        let opened = match self.hub.open_graph_run_set(command).await {
            Ok(GraphRunSetOpenOutcome::Committed { opened, .. })
            | Ok(GraphRunSetOpenOutcome::IdempotentReplay { opened }) => opened,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_graph_run_set_opened(request_id, opened)
    }

    fn respond_graph_run_set_opened(
        &self,
        request_id: RequestId,
        opened: OpenedGraphRunSet,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphRunSetOpen {
                session_id: opened.session_id,
                run_set_id: opened.run_set_id,
                root_graph_id: opened.root_graph_id,
                plan_item_id: opened.plan_item_id,
                plan_event_seq: opened.plan_event_seq,
                template: opened.template,
                digest: opened.digest,
                run_set_opened_seq: opened.run_set_opened_seq,
                through_seq: opened.through_seq,
                children: opened
                    .children
                    .into_iter()
                    .map(|child| TodoGraphOpenedWire {
                        todo_id: child.todo_id,
                        depends_on_todo_id: child.depends_on_todo_id,
                        child_graph_id: child.child_graph_id,
                        attached_seq: child.attached_seq,
                        pinned_seq: child.pinned_seq,
                        opened_seq: child.opened_seq,
                    })
                    .collect(),
                worker_generation: opened.worker_generation,
            },
        })
    }

    async fn graph_switch(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        old_graph_id: GraphId,
        template: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "graph switch needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "old_graph_id": &old_graph_id,
            "template": &template,
        }))
        .map_err(|error| SessionHubError::Task(format!("cannot encode graph switch: {error}")))?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .graph_switch_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(switched)) => return self.respond_graph_switched(request_id, switched),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "graph switch requires a control attachment to this session",
                false,
                None,
            );
        }
        let command = GraphSwitchCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            old_graph_id,
            new_graph_id: GraphId::new(random_id("graph")?),
            template,
            template_spec: None,
            device_id: self.hub.inner.device_id.clone(),
        };
        let switched = match self.hub.switch_graph(command).await {
            Ok(GraphSwitchOutcome::Committed { switched, .. })
            | Ok(GraphSwitchOutcome::IdempotentReplay { switched }) => switched,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_graph_switched(request_id, switched)
    }

    fn respond_graph_switched(
        &self,
        request_id: RequestId,
        switched: SwitchedGraph,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphSwitch {
                session_id: switched.session_id,
                old_graph_id: switched.old_graph_id,
                new_graph_id: switched.new_graph_id,
                template: switched.template,
                digest: switched.digest,
                superseded_seq: switched.superseded_seq,
                pinned_seq: switched.pinned_seq,
                opened_seq: switched.opened_seq,
                worker_generation: switched.worker_generation,
            },
        })
    }

    async fn graph_abandon(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        why: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "graph abandon needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "why": &why,
        }))
        .map_err(|error| SessionHubError::Task(format!("cannot encode graph abandon: {error}")))?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .graph_abandon_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(abandoned)) => return self.respond_graph_abandoned(request_id, abandoned),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        // Same receipt-first ordering as graph.pin: recovery is unfenced;
        // only a fresh mutation must still hold the session control lease.
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "graph abandon requires a control attachment to this session",
                false,
                None,
            );
        }
        let command = GraphAbandonCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            why,
            device_id: self.hub.inner.device_id.clone(),
        };
        let abandoned = match self.hub.abandon_graph(command).await {
            Ok(GraphAbandonOutcome::Committed { abandoned, .. })
            | Ok(GraphAbandonOutcome::IdempotentReplay { abandoned }) => abandoned,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_graph_abandoned(request_id, abandoned)
    }

    fn respond_graph_abandoned(
        &self,
        request_id: RequestId,
        abandoned: AbandonedGraph,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphAbandon {
                session_id: abandoned.session_id,
                graph_id: abandoned.graph_id,
                abandoned_seq: abandoned.abandoned_seq,
                worker_generation: abandoned.worker_generation,
            },
        })
    }

    fn respond_graph_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        let code = match error.code {
            ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
            ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
            ErrorCode::GraphAlreadyActive => ERROR_CODE_GRAPH_ALREADY_ACTIVE,
            ErrorCode::GraphNotActive => ERROR_CODE_GRAPH_NOT_ACTIVE,
            ErrorCode::GraphWrongNode => ERROR_CODE_GRAPH_WRONG_NODE,
            _ => ERROR_CODE_INVALID_ARGUMENT,
        };
        self.respond_error(request_id, code, &error.message, error.retryable, None)
    }

    /// G2 auto-title: on the FIRST main-timeline accept of an untitled
    /// session, journal the same `session_renamed` fact through the same
    /// actor/store lane with an INTERNAL per-session command id — the
    /// receipt makes it at-most-once forever, and the store-side
    /// `only_if_untitled` guard makes overwrite impossible even under an
    /// explicit-rename race. Best-effort by design: a failed auto-title
    /// must never fail the already-committed turn.
    async fn maybe_auto_title(&self, session_id: &SessionId, slug: String) {
        let Ok(Some(metadata)) = self.hub.session_metadata(session_id).await else {
            return;
        };
        if metadata.title.is_some() {
            return;
        }
        // Generation- and title-free coordinates: the SAME digest across
        // retries and daemon restarts, so the receipt dedupes forever.
        let Ok(request_json) = serde_json::to_string(&serde_json::json!({
            "session_id": session_id,
            "auto_title": true,
        })) else {
            return;
        };
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let Ok(event_id) = random_id("auto-title") else {
            return;
        };
        let command = SessionRenameCommand {
            command_id: format!("auto-title-{session_id}"),
            request_digest,
            request_json,
            session_id: session_id.clone(),
            worker_generation: self.hub.inner.store.worker_generation(),
            title: Some(slug),
            only_if_untitled: true,
            event_id: EventId::new(event_id),
            device_id: self.hub.inner.device_id.clone(),
        };
        let _ = self.hub.rename_session(command).await;
    }

    /// `session.select_effort` — receipted live-session effort selection
    /// (G3), the exact `session.select_model` law set: receipt replay
    /// precedes validation, the ONE authority in `crate::model_select`
    /// validates against the CURRENT pair's declared ladder, the store owns
    /// durability, and the next logical turn re-reads the committed
    /// metadata (R6 re-resolution).
    async fn session_select_effort(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        effort: Option<String>,
        confirm_new_epoch: bool,
    ) -> Result<(), SessionHubError> {
        let effort = effort
            .map(|effort| effort.trim().to_owned())
            .filter(|effort| !effort.is_empty());
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "effort selection needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "effort": &effort,
            "confirm_new_epoch": confirm_new_epoch,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!(
                "cannot encode effort-selection coordinates: {error}"
            ))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt replay precedes validation so a lost response remains
        // recoverable even after registry or inventory changes.
        match self
            .hub
            .session_select_effort_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(selected)) => return self.respond_effort_selected(request_id, selected),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        let Some(current) = (match self.hub.session_metadata(&session_id).await {
            Ok(metadata) => metadata,
            Err(error) => return self.respond_turn_error(request_id, error),
        }) else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "effort selection requires a live session with typed metadata",
                false,
                None,
            );
        };
        let authority = self.tuning_authority()?;
        if let Err(refusal) =
            authority.validate_effort(&current.provider, &current.model, effort.as_deref())
        {
            return self.respond_tuning_refusal(request_id, &refusal);
        }
        let changed_fields = if current.effort != effort {
            vec!["effort/thinking".to_owned()]
        } else {
            Vec::new()
        };
        let current_scope =
            crate::cache_policy::latest_main_cache_scope(&self.hub.inner.store, &session_id)
                .await?;
        if let Some(warning) = crate::cache_policy::assess_cache_change(
            &current,
            current_scope.as_ref(),
            &current.provider,
            &current.model,
            current_scope
                .as_ref()
                .map(|scope| scope.auth_scope.as_str()),
            changed_fields,
            true,
        ) && crate::cache_policy::blocks_change(&warning, confirm_new_epoch)
        {
            return self.respond_cache_confirmation_required(request_id, &warning);
        }

        let command = haider_core::SessionSelectEffortCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            effort,
            event_id: EventId::new(random_id("effort-selected")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.select_session_effort(command).await {
            Ok(SessionSelectEffortOutcome::Committed { selected, .. })
            | Ok(SessionSelectEffortOutcome::IdempotentReplay { selected }) => selected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_effort_selected(request_id, selected)
    }

    /// `session.select_fast` — the receipted fast-mode toggle (G3), same
    /// law set as `session.select_effort`.
    async fn session_select_fast(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        enabled: bool,
        confirm_new_epoch: bool,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "fast-mode selection needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "enabled": enabled,
            "confirm_new_epoch": confirm_new_epoch,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!(
                "cannot encode fast-mode-selection coordinates: {error}"
            ))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        match self
            .hub
            .session_select_fast_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(selected)) => return self.respond_fast_selected(request_id, selected),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        let Some(current) = (match self.hub.session_metadata(&session_id).await {
            Ok(metadata) => metadata,
            Err(error) => return self.respond_turn_error(request_id, error),
        }) else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "fast-mode selection requires a live session with typed metadata",
                false,
                None,
            );
        };
        let authority = self.tuning_authority()?;
        if let Err(refusal) = authority.validate_fast(&current.provider, &current.model, enabled) {
            return self.respond_tuning_refusal(request_id, &refusal);
        }
        let changed_fields = if current.fast != enabled {
            vec!["fast/speed".to_owned()]
        } else {
            Vec::new()
        };
        let current_scope =
            crate::cache_policy::latest_main_cache_scope(&self.hub.inner.store, &session_id)
                .await?;
        if let Some(warning) = crate::cache_policy::assess_cache_change(
            &current,
            current_scope.as_ref(),
            &current.provider,
            &current.model,
            current_scope
                .as_ref()
                .map(|scope| scope.auth_scope.as_str()),
            changed_fields,
            true,
        ) && crate::cache_policy::blocks_change(&warning, confirm_new_epoch)
        {
            return self.respond_cache_confirmation_required(request_id, &warning);
        }

        let command = haider_core::SessionSelectFastCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            enabled,
            event_id: EventId::new(random_id("fast-selected")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.select_session_fast(command).await {
            Ok(SessionSelectFastOutcome::Committed { selected, .. })
            | Ok(SessionSelectFastOutcome::IdempotentReplay { selected }) => selected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_fast_selected(request_id, selected)
    }

    /// The one selection authority, loaded with the same summaries
    /// `session.select_model` consults.
    fn tuning_authority(
        &self,
    ) -> Result<crate::model_select::ModelSelectionAuthority, SessionHubError> {
        let summaries = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read())
            .map(|view| view.providers)
            .unwrap_or_default();
        Ok(crate::model_select::ModelSelectionAuthority::new(
            self.hub.creatable_providers()?,
            summaries,
        ))
    }

    fn respond_tuning_refusal(
        &self,
        request_id: RequestId,
        refusal: &crate::model_select::TuningRefusal,
    ) -> Result<(), SessionHubError> {
        use crate::model_select::TuningRefusal;
        let (code, data) = match refusal {
            TuningRefusal::EffortUnsupported {
                provider,
                model,
                effort,
                supported,
            } => (
                haider_rpc::ERROR_CODE_EFFORT_UNSUPPORTED,
                Some(ErrorData::EffortUnsupported {
                    provider: provider.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                    supported: supported.clone(),
                }),
            ),
            TuningRefusal::FastUnsupported { provider, model } => (
                haider_rpc::ERROR_CODE_FAST_UNSUPPORTED,
                Some(ErrorData::FastUnsupported {
                    provider: provider.clone(),
                    model: model.clone(),
                }),
            ),
        };
        self.respond_error(request_id, code, &refusal.message(), false, data)
    }

    fn respond_cache_confirmation_required(
        &self,
        request_id: RequestId,
        warning: &crate::cache_policy::CacheChangeWarning,
    ) -> Result<(), SessionHubError> {
        let policy = match warning.policy {
            haider_protocol::cache::CachePolicyMode::Economy => "economy",
            haider_protocol::cache::CachePolicyMode::Balanced => "balanced",
            haider_protocol::cache::CachePolicyMode::Mobility => "mobility",
        };
        self.respond_error(
            request_id,
            haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED,
            &warning.message(),
            false,
            Some(ErrorData::CacheEpochConfirmationRequired {
                changed_fields: warning.changed_fields.clone(),
                invalidated_stable_tokens: warning.invalidated_stable_tokens,
                rewarm_cost_microusd: warning.rewarm_cost_microusd,
                rewarm_api_equivalent_cost_microusd: warning.rewarm_api_equivalent_cost_microusd,
                rewarm_base_input_equivalent_tokens: warning.rewarm_base_input_equivalent_tokens,
                policy: policy.to_owned(),
            }),
        )
    }

    fn respond_effort_selected(
        &self,
        request_id: RequestId,
        selected: SelectedEffort,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSelectEffort {
                session_id: selected.session_id,
                effort: selected.effort,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }

    fn respond_fast_selected(
        &self,
        request_id: RequestId,
        selected: SelectedFast,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSelectFast {
                session_id: selected.session_id,
                enabled: selected.enabled,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }

    async fn agent_message(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        agent: haider_protocol::ids::AgentId,
        text: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "agent-message command id must not be empty",
                false,
                None,
            );
        }
        if worker_generation != self.hub.worker_generation() {
            return self.respond_error(
                request_id,
                ERROR_CODE_STALE_GENERATION,
                "agent-message worker generation is stale",
                false,
                None,
            );
        }
        let message = match MessageSubagent::from_tool_args(serde_json::json!({
            "agent": agent,
            "message": text,
        })) {
            Ok(message) => message,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.to_string(),
                    false,
                    None,
                );
            }
        };
        let delegation = DelegationHandle::new(self.hub.clone());
        let parent_agent_id = match delegation.agent_for_session(&session_id).await {
            Ok(agent) => agent,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        let receipt = match delegation
            .message(
                MessageCoordinates {
                    parent_session_id: session_id,
                    parent_agent_id,
                    command_id: command_id.0,
                },
                message,
            )
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::AgentMessage { receipt },
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn_submit(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        branch_id: Option<haider_protocol::ids::BranchId>,
        text: String,
        attachments: Vec<haider_protocol::tool::AttachmentBlock>,
        mode: haider_protocol::DeliveryMode,
        trust_hooks: bool,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() || text.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "turn command id and text must not be empty",
                false,
                None,
            );
        }
        let mut request_value = serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "branch_id": &branch_id,
            "text": &text,
            "attachments": &attachments,
            "mode": mode,
        });
        if trust_hooks {
            let Some(request) = request_value.as_object_mut() else {
                return Err(SessionHubError::Task(
                    "turn-submit coordinates did not encode as an object".into(),
                ));
            };
            request.insert("trust_hooks".into(), serde_json::Value::Bool(true));
        }
        let request_json = serde_json::to_string(&request_value).map_err(|error| {
            SessionHubError::Task(format!("cannot encode turn-submit coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .turn_accept_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(accepted)) => {
                // G2: a replayed first-accept still gets its auto-title —
                // the internal receipt makes the retry harmless, and a
                // crash between the original accept and its auto-title
                // would otherwise leave the session unnamed forever.
                if accepted.first_user_turn {
                    self.maybe_auto_title(&session_id, auto_title_slug(&text))
                        .await;
                }
                if accepted.worker_generation == self.hub.inner.store.worker_generation() {
                    let handoff = match accepted.disposition {
                        TurnAdmissionDisposition::SteerPending => {
                            self.hub
                                .submit_internal_nudge(accepted.clone(), text.clone())
                                .await
                        }
                        TurnAdmissionDisposition::SubturnPending => {
                            self.hub
                                .submit_internal_subturn(accepted.clone(), text.clone())
                                .await
                        }
                        TurnAdmissionDisposition::Started | TurnAdmissionDisposition::Queued => {
                            self.hub.worker_manager()?.submit(accepted.clone()).await
                        }
                    };
                    if let Err(error) = handoff {
                        return self.respond_turn_error(request_id, error);
                    }
                }
                return self.respond_turn_accepted(request_id, accepted);
            }
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        let pdf_delivery = if attachments.iter().any(|attachment| {
            matches!(
                attachment,
                haider_protocol::tool::AttachmentBlock::Pdf { .. }
            )
        }) {
            let metadata = match self.hub.session_metadata(&session_id).await {
                Ok(Some(metadata)) => metadata,
                Ok(None) => {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "PDF attachment admission requires typed session metadata",
                        false,
                        None,
                    );
                }
                Err(error) => return self.respond_turn_error(request_id, error),
            };
            pdf_delivery_for_provider(&metadata.provider)
        } else {
            haider_protocol::tool::PdfDeliveryMode::ExtractedText
        };
        let attachments = match validate_turn_attachments(
            &self.hub.inner.store,
            &attachments,
            pdf_delivery,
        )
        .await
        {
            Ok(attachments) => attachments,
            Err(failure) => {
                return self.respond_error(
                    request_id,
                    failure.code,
                    &failure.message,
                    false,
                    failure.data,
                );
            }
        };
        // Captured before `text` moves into the acceptance command; only a
        // committed FIRST accept consumes it (G2 auto-title).
        let first_turn_slug = auto_title_slug(&text);
        let delivery_text = text.clone();
        let command = TurnAcceptCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id: session_id.clone(),
            worker_generation,
            run_id: haider_protocol::ids::RunId::new(random_id("run")?),
            agent_id: None,
            branch_id,
            text,
            attachments,
            mode,
            queued_event_id: EventId::new(random_id("turn-queued")?),
            user_event_id: EventId::new(random_id("turn-user")?),
            active_event_id: EventId::new(random_id("session-active")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let accepted = match self.hub.accept_turn(command).await {
            Ok(TurnAcceptOutcome::Committed { accepted, .. })
            | Ok(TurnAcceptOutcome::IdempotentReplay { accepted }) => accepted,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        // G2 auto-title, between the committed acceptance and the worker
        // handoff so the config fact lands ahead of any run movement (the
        // F3 head-CAS tolerance covers a later interleave anyway).
        if accepted.first_user_turn {
            self.maybe_auto_title(&accepted.session_id, first_turn_slug)
                .await;
        }
        // Durable-before-provider: the manager sees this only after the actor
        // committed and synchronously published the acceptance transaction.
        let handoff = match accepted.disposition {
            TurnAdmissionDisposition::SteerPending => {
                self.hub
                    .submit_internal_nudge(accepted.clone(), delivery_text)
                    .await
            }
            TurnAdmissionDisposition::SubturnPending => {
                self.hub
                    .submit_internal_subturn(accepted.clone(), delivery_text)
                    .await
            }
            TurnAdmissionDisposition::Started | TurnAdmissionDisposition::Queued => {
                self.hub.worker_manager()?.submit(accepted.clone()).await
            }
        };
        if let Err(error) = handoff {
            return self.respond_turn_error(request_id, error);
        }
        self.respond_turn_accepted(request_id, accepted)
    }

    #[allow(clippy::too_many_arguments)]
    async fn shell_exec(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        command: String,
        cwd: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() || command.trim().is_empty() || command.len() > 8_192 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "shell command id and 1..=8192 UTF-8 command bytes are required",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "command": &command,
            "cwd": &cwd,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode shell-exec coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .shell_exec_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(accepted)) => {
                if accepted.worker_generation == self.hub.inner.store.worker_generation()
                    && let Err(error) = self
                        .hub
                        .worker_manager()?
                        .shell_exec(accepted.clone(), command_id.0.clone(), command, cwd)
                        .await
                {
                    return self.respond_shell_error(request_id, error);
                }
                return self.respond_shell_accepted(request_id, accepted);
            }
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_shell_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        let trimmed = command.trim();
        if trimmed == "cd"
            || trimmed
                .strip_prefix("cd")
                .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_whitespace))
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN,
                "`!cd` is unsupported: daemon-owned persistent shell cwd is a later design",
                false,
                None,
            );
        }
        if let Some(cwd) = cwd.as_deref()
            && (cwd.is_empty() || std::path::Path::new(cwd).is_absolute())
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "shell cwd must be a non-empty workspace-relative path",
                false,
                None,
            );
        }
        let accepted = match self
            .hub
            .accept_shell_exec(ShellExecAcceptCommand {
                command_id: command_id.0.clone(),
                request_digest,
                request_json,
                session_id: session_id.clone(),
                worker_generation,
                run_id: RunId::new(random_id("shell-run")?),
                item_id: haider_protocol::ids::ItemId::new(random_id("shell-item")?),
                command: command.clone(),
                running_event_id: EventId::new(random_id("shell-running")?),
                item_event_id: EventId::new(random_id("shell-item-started")?),
                active_event_id: EventId::new(random_id("shell-session-active")?),
                device_id: self.hub.inner.device_id.clone(),
            })
            .await
        {
            Ok(ShellExecAcceptOutcome::Committed { accepted, .. })
            | Ok(ShellExecAcceptOutcome::IdempotentReplay { accepted }) => accepted,
            Err(SessionHubError::Store(error)) => {
                return self.respond_shell_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = self
            .hub
            .worker_manager()?
            .shell_exec(accepted.clone(), command_id.0, command, cwd)
            .await
        {
            return self.respond_shell_error(request_id, error);
        }
        self.respond_shell_accepted(request_id, accepted)
    }

    async fn tools_inventory(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let inventory =
            crate::worker::tool_inventory_snapshot(&self.hub.inner.store, &session_id).await?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ToolsInventory {
                session_id,
                inventory,
            },
        })
    }

    /// Cross-provider usage snapshot (U1). No installed service is an honest
    /// empty report (mirrors the missing-facade `account.list` answer), and
    /// per-account meter failures NEVER fail the frame — they ride as typed
    /// unavailability inside the report.
    async fn usage_report(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(service) = self.hub.usage_report_service()? else {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::UsageReport {
                    report: haider_protocol::usage::UsageReportV1 {
                        generated_at_ms: 0,
                        accounts: Vec::new(),
                    },
                },
            });
        };
        let report = service.report(&self.hub.inner.store).await?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::UsageReport { report },
        })
    }

    async fn session_compact(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        branch_id: Option<BranchId>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-compact command id must not be empty",
                false,
                None,
            );
        }
        let accepted = match self
            .hub
            .worker_manager()?
            .compact(
                session_id.clone(),
                command_id.0,
                worker_generation,
                branch_id,
            )
            .await
        {
            Ok(accepted) => accepted,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        let body = if let Some(branch_id) = accepted.branch_id {
            ResponseBody::SessionCompactOnBranch {
                session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                branch_id,
            }
        } else {
            ResponseBody::SessionCompact {
                session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
            }
        };
        self.send(WireFrame::Response { request_id, body })
    }

    async fn turn_cancel(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        run_id: haider_protocol::ids::RunId,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "turn-cancel command id must not be empty",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "run_id": &run_id,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode turn-cancel coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let cancelled = match self
            .hub
            .turn_cancel_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(cancelled)) => cancelled,
            Ok(None) => {
                let command = TurnCancelCommand {
                    command_id: command_id.0,
                    request_digest,
                    request_json,
                    session_id: session_id.clone(),
                    worker_generation,
                    run_id: run_id.clone(),
                    cancelling_event_id: EventId::new(random_id("turn-cancelling")?),
                    device_id: self.hub.inner.device_id.clone(),
                };
                match self.hub.cancel_turn(command).await {
                    Ok(TurnCancelOutcome::Committed { cancelled, .. })
                    | Ok(TurnCancelOutcome::IdempotentReplay { cancelled }) => cancelled,
                    Err(SessionHubError::Store(error)) => {
                        return self.respond_turn_error(request_id, error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::TurnCancel {
                session_id: cancelled.session_id,
                run_id: cancelled.run_id,
                status: match cancelled.status {
                    TurnCancellationStatus::Accepted => CancelStatus::Accepted,
                    TurnCancellationStatus::AlreadyTerminal => CancelStatus::AlreadyTerminal,
                },
                terminal_seq: cancelled.terminal_seq,
            },
        })
    }

    fn respond_turn_accepted(
        &self,
        request_id: RequestId,
        accepted: AcceptedTurn,
    ) -> Result<(), SessionHubError> {
        let disposition = match accepted.disposition {
            TurnAdmissionDisposition::Started => SubmitDisposition::Started,
            TurnAdmissionDisposition::Queued => SubmitDisposition::Queued,
            TurnAdmissionDisposition::SteerPending => SubmitDisposition::SteerPending,
            TurnAdmissionDisposition::SubturnPending => SubmitDisposition::SubturnPending,
        };
        let body = if let Some(branch_id) = accepted.branch_id {
            ResponseBody::TurnSubmitOnBranch {
                session_id: accepted.session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                branch_id,
                disposition,
            }
        } else {
            ResponseBody::TurnSubmit {
                session_id: accepted.session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                disposition,
            }
        };
        self.send(WireFrame::Response { request_id, body })
    }

    fn respond_shell_accepted(
        &self,
        request_id: RequestId,
        accepted: AcceptedShellExec,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ShellExec {
                session_id: accepted.session_id,
                item_id: accepted.item_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
            },
        })
    }

    fn respond_shell_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        let code = match error.code {
            ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
            ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
            ErrorCode::Busy => ERROR_CODE_BUSY,
            ErrorCode::RunNotActive => ERROR_CODE_RUN_NOT_ACTIVE,
            _ => ERROR_CODE_INVALID_ARGUMENT,
        };
        self.respond_error(request_id, code, &error.message, error.retryable, None)
    }

    fn respond_turn_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        let code = match error.code {
            ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
            ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
            ErrorCode::RunNotActive => ERROR_CODE_RUN_NOT_ACTIVE,
            ErrorCode::Busy => ERROR_CODE_OVERLOADED,
            ErrorCode::VisionUnsupported => ERROR_CODE_VISION_UNSUPPORTED,
            _ => ERROR_CODE_INVALID_ARGUMENT,
        };
        self.respond_error(request_id, code, &error.message, error.retryable, None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn session_create(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        cwd: String,
        provider: String,
        model: String,
        max_tokens: u64,
        permission_overrides: Option<haider_protocol::session::SessionPermissionOverridesV1>,
        cache_policy: haider_protocol::cache::CachePolicySettingsV1,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-create command id must not be empty",
                false,
                None,
            );
        }
        let mut request_coordinates = serde_json::json!({
            "cwd": &cwd,
            "provider": &provider,
            "model": &model,
            "max_tokens": max_tokens,
        });
        if let Some(overrides) = permission_overrides {
            request_coordinates["permission_overrides"] =
                serde_json::to_value(overrides).map_err(|error| {
                    SessionHubError::Task(format!(
                        "cannot encode session permission overrides: {error}"
                    ))
                })?;
        }
        if !cache_policy.is_default() {
            request_coordinates["cache_policy"] =
                serde_json::to_value(cache_policy).map_err(|error| {
                    SessionHubError::Task(format!("cannot encode session cache policy: {error}"))
                })?;
        }
        let request_json = serde_json::to_string(&request_coordinates).map_err(|error| {
            SessionHubError::Task(format!("cannot encode session-create coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt lookup deliberately precedes path validation. A response
        // lost after commit remains recoverable even if the workspace was
        // deleted before the retry reached a new connection.
        match self
            .hub
            .session_create_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(created)) => return self.respond_created(request_id, created),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.message,
                    error.retryable,
                    None,
                );
            }
            Err(error) => return Err(error),
        }

        // D3-5: the dependency configuration is the ONE authority on
        // creatable providers. Production answers the built-in adapter set;
        // "fake" exists only under injected test configurations. Since
        // W5g-5 an ENABLED custom chat-completions profile is creatable
        // too — it exists only because a durable, validated
        // `provider.configure` committed it, and the turn path routes it
        // by family.
        let creatable = self.hub.creatable_providers()?;
        let static_creatable = creatable
            .as_ref()
            .is_some_and(|providers| providers.contains(provider.as_str()));
        let custom_creatable = || {
            self.hub.accounts().ok().flatten().is_some_and(|facade| {
                facade.management.read().is_some_and(|view| {
                    view.providers.iter().any(|profile| {
                        profile.provider == provider
                            && profile.enabled
                            && matches!(
                                profile.api_family,
                                haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions
                            )
                    })
                })
            })
        };
        if !static_creatable && !custom_creatable() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unsupported session provider",
                false,
                None,
            );
        }
        const MAX_DAEMON_OUTPUT_RESERVE: u64 = 30_000;
        if model.trim().is_empty() || max_tokens == 0 || max_tokens > MAX_DAEMON_OUTPUT_RESERVE {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session model must be non-empty and max_tokens must be in 1..=30000",
                false,
                None,
            );
        }
        let workspace = match validate_workspace(cwd).await {
            Ok(workspace) => workspace,
            Err(message) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &message,
                    false,
                    None,
                );
            }
        };
        let command = SessionCreateCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id: SessionId::new(random_id("session")?),
            cwd: workspace.canonical,
            provider,
            model,
            max_tokens,
            permission_overrides,
            effort: None,
            fast: false,
            cache_policy,
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(random_id("session-created")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        // Keep the opened directory descriptor alive until the transaction
        // returns. M3 transfers the same canonical identity into its broker.
        let _descriptor = workspace.descriptor;
        match self.hub.create_session(command).await {
            Ok(SessionCreateOutcome::Committed { created, .. })
            | Ok(SessionCreateOutcome::IdempotentReplay { created }) => {
                self.respond_created(request_id, created)
            }
            Err(SessionHubError::Store(error)) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &error.message,
                error.retryable,
                None,
            ),
            Err(error) => Err(error),
        }
    }

    fn respond_created(
        &self,
        request_id: RequestId,
        created: CreatedSession,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionCreate {
                session_id: created.session_id,
                created_seq: created.created_seq,
                worker_generation: created.worker_generation,
                metadata: created.metadata,
            },
        })
    }

    async fn session_list(
        &self,
        request_id: RequestId,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<(), SessionHubError> {
        let after = match cursor.as_deref().map(decode_cursor).transpose() {
            Ok(after) => after,
            Err(()) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_CURSOR,
                    "session-list cursor is invalid",
                    false,
                    None,
                );
            }
        };
        let limit = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(MAX_LIST_PAGE);
        if limit == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-list limit must be greater than zero",
                false,
                None,
            );
        }
        let ids = self.hub.inner.store.session_ids().await?;
        let mut selected = ids
            .into_iter()
            .filter(|session_id| {
                after
                    .as_ref()
                    .is_none_or(|after| session_id.as_str() > after.as_str())
            })
            .take(limit.saturating_add(1))
            .collect::<Vec<_>>();
        let has_more = selected.len() > limit;
        if has_more {
            selected.truncate(limit);
        }
        let mut sessions = Vec::with_capacity(selected.len());
        for session_id in &selected {
            let head_seq = self.hub.inner.store.latest_seq(session_id).await?;
            let metadata = self.hub.inner.store.session_metadata(session_id).await?;
            // Roster truth for unattached sessions: replay the same sealed
            // journal the observe surface reads. The launcher must never
            // show "0 turns · 0 tok" for a session that merely lacks an
            // attachment.
            let (turns, footprint) =
                session_summary_truth(&self.hub.inner.store, session_id, head_seq).await?;
            let (footprint_tokens, footprint_truth) =
                summary_footprint_fields(turns, footprint.as_ref());
            let initial_model = metadata
                .as_ref()
                .map_or("", |metadata| metadata.model.as_str());
            let agent_metrics = session_agent_metrics_truth(
                &self.hub.inner.store,
                session_id,
                head_seq,
                initial_model,
            )
            .await?;
            // G2: the committed title rides the summary top-level so
            // rosters name rows without decoding metadata.
            let title = metadata
                .as_ref()
                .and_then(|metadata| metadata.title.clone());
            sessions.push(SessionSummary {
                session_id: session_id.clone(),
                head_seq,
                worker_generation: self.hub.inner.store.worker_generation(),
                workspace_cwd: metadata.as_ref().map(|metadata| metadata.cwd.clone()),
                metadata,
                turn_count: Some(turns),
                footprint_tokens,
                footprint_truth,
                title,
                agent_metrics,
            });
        }
        let next_cursor = has_more
            .then(|| selected.last().map(encode_cursor))
            .flatten();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionList {
                sessions,
                next_cursor,
            },
        })
    }

    async fn session_read(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        range: SeqRange,
    ) -> Result<(), SessionHubError> {
        let head = self.hub.inner.store.latest_seq(&session_id).await?;
        if head == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        if range.start_seq == 0 || range.end_seq < range.start_seq {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-read range must be non-empty and start at sequence one or later",
                false,
                None,
            );
        }
        let count = range
            .end_seq
            .saturating_sub(range.start_seq)
            .saturating_add(1);
        let limit = usize::try_from(count).unwrap_or(usize::MAX);
        if limit > MAX_READ_ENVELOPES {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-read range exceeds the maximum of 1024 envelopes",
                false,
                None,
            );
        }
        let envelopes = self
            .hub
            .inner
            .store
            .read(&session_id, range.start_seq.saturating_sub(1), limit)
            .await?
            .into_iter()
            .take_while(|envelope| envelope.seq <= range.end_seq)
            .collect::<Vec<_>>();
        let latest_context_footprint =
            latest_context_footprint(&self.hub.inner.store, &session_id, head).await?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionRead {
                result: SessionReadResult {
                    metadata: self.hub.inner.store.session_metadata(&session_id).await?,
                    session_id,
                    range,
                    head_seq: head,
                    latest_context_footprint,
                    envelopes,
                },
            },
        })
    }

    async fn session_observe(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        last_event_limit: u32,
    ) -> Result<(), SessionHubError> {
        const MAX_EVENT_KINDS: usize = 100;

        let head = self.hub.inner.store.latest_seq(&session_id).await?;
        if head == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let event_limit = usize::try_from(last_event_limit)
            .unwrap_or(usize::MAX)
            .min(MAX_EVENT_KINDS);
        let metadata = self.hub.inner.store.session_metadata(&session_id).await?;
        let mut projection = ObserveProjection::new(event_limit);
        let mut cursor = 0;
        while cursor < head {
            let page = self
                .hub
                .inner
                .store
                .read(&session_id, cursor, REPLAY_PAGE_SIZE)
                .await?;
            if page.is_empty() {
                break;
            }
            let mut advanced = false;
            for envelope in page {
                if envelope.seq > head {
                    break;
                }
                cursor = envelope.seq;
                advanced = true;
                projection.apply(envelope);
            }
            if !advanced {
                break;
            }
        }
        let digest = projection.finish(
            session_id,
            head,
            self.hub.inner.store.worker_generation(),
            metadata,
        );
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionObserve { digest },
        })
    }

    async fn session_fleet(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let bounded = self
            .hub
            .delegation_descendants(
                session_id.clone(),
                haider_rpc::FLEET_MAX_NODES as usize,
                haider_rpc::FLEET_MAX_DEPTH,
            )
            .await?;
        let generated_at_ms = fleet_now_ms();
        let mut nodes = Vec::with_capacity(bounded.descendants.len());
        for descendant in bounded.descendants {
            let direct_child_count = descendant.direct_child_count;
            let record = descendant.record;
            let head_seq = self
                .hub
                .inner
                .store
                .latest_seq(&record.child_session_id)
                .await?;
            let initial_model = self
                .hub
                .inner
                .store
                .session_metadata(&record.child_session_id)
                .await?
                .map_or_else(String::new, |metadata| metadata.model);
            let truth =
                fleet_child_truth(&self.hub.inner.store, &record, head_seq, &initial_model).await?;
            nodes.push(FleetFlatNode {
                record,
                state: truth.state,
                metrics: truth.metrics,
                direct_child_count,
            });
        }
        let snapshot = fleet_snapshot(session_id, generated_at_ms, nodes, bounded.truncated)?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionFleet { snapshot },
        })
    }

    async fn session_diagnostic(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        code: String,
        message: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty()
            || code != "client-daemon-incompatible"
            || message.trim().is_empty()
            || message.len() > 1_024
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session diagnostic coordinates are invalid",
                false,
                None,
            );
        }
        let mut cursor = 0_u64;
        let mut last = None;
        loop {
            let page = self
                .hub
                .inner
                .store
                .read(&session_id, cursor, REPLAY_PAGE_SIZE)
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if let Ok(EventPayload::ClientDiagnostic {
                    command_id: existing,
                    code: existing_code,
                    message: existing_message,
                }) = serde_json::from_value(envelope.payload.clone())
                    && existing == command_id.as_str()
                {
                    if existing_code != code || existing_message != message {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "session diagnostic command id was reused with different content",
                            false,
                            None,
                        );
                    }
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::SessionDiagnostic {
                            recorded_seq: envelope.seq,
                        },
                    });
                }
                last = Some(envelope);
            }
        }
        let Some(last) = last else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        };
        let payload = EventPayload::ClientDiagnostic {
            command_id: command_id.as_str().to_owned(),
            code,
            message,
        };
        let mut envelopes = [haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: EventId::new(format!(
                "client-diagnostic-{}",
                blake3::hash(command_id.as_str().as_bytes()).to_hex()
            )),
            seq: 0,
            session_id,
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: self.hub.inner.device_id.clone(),
            authority_epoch: last.authority_epoch,
            worker_generation: self.hub.inner.store.worker_generation(),
            causation_id: Some(last.event_id),
            correlation_id: None,
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).map_err(|error| {
                SessionHubError::Task(format!("cannot encode session diagnostic: {error}"))
            })?,
        }];
        let recorded = match self.hub.append(&mut envelopes).await {
            Ok(_) => envelopes[0].seq,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionDiagnostic {
                recorded_seq: recorded,
            },
        })
    }

    async fn session_attach(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let registration = match self
            .hub
            .register(&self.connection_id, session_id, after_seq, mode)
            .await?
        {
            RegisterResult::Registered(registration) => registration,
            RegisterResult::CursorAhead { requested, head } => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_CURSOR_AHEAD,
                    "replay cursor is beyond the committed session head",
                    false,
                    Some(ErrorData::CursorAhead { requested, head }),
                );
            }
            // Same stable code the connection cap uses (its doc names
            // admission caps as the family); correlated and retryable here.
            RegisterResult::Overloaded { message } => {
                return self.respond_error(request_id, ERROR_CODE_OVERLOADED, &message, true, None);
            }
        };
        let attachment_id = registration.attachment_id.clone();
        let attach_state = registration.attach_state.clone();
        // Close-vs-registration sweep (P2-4): `close` sets `closed` BEFORE
        // it snapshots the owners map, so a registration that landed after
        // that snapshot always observes `closed` here and detaches itself;
        // one that landed before it was swept by close. Either way no
        // attachment survives on a closed connection.
        if self.closed.load(Ordering::Acquire) {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(SessionHubError::Closed);
        }
        // Response-before-first-event: the response is staged with a marker
        // that gates this attachment's event offers until it has left the
        // queue, so no replayed event can precede the response that names
        // the attachment id (and a purge that still finds it answers the
        // request — see the unknown-id rule on `lag_and_detach`).
        if self
            .sink
            .try_send_for(
                &attachment_id,
                WireFrame::Response {
                    request_id,
                    body: ResponseBody::SessionAttach {
                        attachment_id: attachment_id.clone(),
                        attach_state,
                    },
                },
            )
            .is_err()
        {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(SessionHubError::Delivery);
        }
        self.hub
            .spawn_replay(registration, after_seq, Arc::clone(&self.sink))
    }

    async fn hooks_list(&self, request_id: RequestId, cwd: String) -> Result<(), SessionHubError> {
        let Some(hooks) = self.hub.hooks()? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "hook service is unavailable",
                false,
                None,
            );
        };
        match hooks.list(std::path::PathBuf::from(cwd)).await {
            Ok((policy, revision, hooks)) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::HooksList {
                    policy: policy.as_str().to_owned(),
                    revision,
                    hooks,
                },
            }),
            Err(message) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &message,
                false,
                None,
            ),
        }
    }

    async fn hooks_trust(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        digest: String,
        trusted: bool,
    ) -> Result<(), SessionHubError> {
        let Some(hooks) = self.hub.hooks()? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "hook service is unavailable",
                false,
                None,
            );
        };
        match hooks.apply_trust(command_id, digest, trusted).await {
            Ok(change) => self.send(WireFrame::Response {
                request_id,
                body: if trusted {
                    ResponseBody::HooksTrust {
                        digest: change.digest,
                        trusted: change.trusted,
                    }
                } else {
                    ResponseBody::HooksRevoke {
                        digest: change.digest,
                        trusted: change.trusted,
                    }
                },
            }),
            Err(error) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &error.message,
                error.retryable,
                None,
            ),
        }
    }

    async fn session_detach(
        &self,
        request_id: RequestId,
        attachment_id: AttachmentId,
    ) -> Result<(), SessionHubError> {
        let owner = self
            .hub
            .take_attachment(&attachment_id, Some(&self.connection_id))?;
        let Some(owner) = owner else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "attachment was not found on this connection",
                false,
                None,
            );
        };
        // Removal/cancellation happened under the same ownership lock used by
        // replay delivery. Purging now is therefore a terminal lane barrier.
        // (The purge cannot report a pending response: the client could only
        // name this attachment id after receiving that response.)
        let _ = self.sink.purge_attachment(&attachment_id);
        SessionHub::finish_detach(&attachment_id, owner).await;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionDetach { attachment_id },
        })
    }

    /// Handles the durable top-level `MenuAnswer` command.
    ///
    /// The arbitration law — first COMMITTED answer wins, losers get the
    /// winner's `resolution_seq` — is stated on
    /// `haider_store::Store::resolve_menu`; this method adds transport
    /// concerns only: capability + attachment policy, wire error mapping, and
    /// the correlated reply. Every attachment learns the outcome from the
    /// event stream (the actor publishes the committed envelope); the reply
    /// is a convenience, never the authority.
    ///
    /// Policy decision (brief §6): answering requires a CONTROL attachment to
    /// the target session — v0.1 has no "controller without a viewport"
    /// allowance.
    #[allow(clippy::too_many_arguments)]
    pub async fn menu_answer(
        &self,
        request_id: Option<RequestId>,
        command_id: CommandId,
        session_id: SessionId,
        menu_id: haider_protocol::ids::MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: String,
        option_index: u32,
        input: Option<MenuInput>,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.menu_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        if let Err(message) = authorize(&self.capabilities, Operation::Control) {
            return self.menu_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                message,
                false,
                None,
            );
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.menu_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "menu answers require a control attachment to this session",
                false,
                None,
            );
        }
        let (value, secret_reference) = match input {
            Some(MenuInput::Text { text }) => (Some(text), false),
            Some(MenuInput::SecretVaultReference { vault_reference }) => {
                (Some(vault_reference), true)
            }
            None => (None, false),
            Some(_) => {
                return self.menu_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "unknown menu input kind",
                    false,
                    None,
                );
            }
        };
        let answer = DurableMenuAnswer {
            menu: menu_id,
            option_key: (!option_key.is_empty()).then_some(option_key),
            option_index,
            value,
            via: AnswerVia::Rpc,
        };
        // Symmetric with `session_attach` (durable existence precedes actor
        // creation), so a bad session id can never mint a permanent actor.
        // Kept after the attachment-policy check to preserve that check's
        // pinned `capability_denied` for unattached callers.
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.menu_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let actor = self.hub.actor_for(session_id.clone()).await?;
        let recovery_answer = answer.clone();
        let recovery_session = session_id.clone();
        let command = MenuResolutionCommand {
            command_id: command_id.0,
            session_id,
            request_seq,
            worker_generation,
            allow_prior_generation: false,
            answer,
            device_id: self.hub.inner.device_id.clone(),
            input_is_secret_reference: secret_reference,
        };
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::MenuAnswer { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        match result.await.map_err(|_| SessionHubError::Closed)? {
            Ok(MenuResolutionOutcome::Committed {
                ref envelope,
                ref menu,
                ..
            }) => {
                if let MenuKind::Recovery { effect, .. } = &menu.kind {
                    let action =
                        super::actor::selected_effect_recovery_action(menu, &recovery_answer);
                    let follow_up = match action {
                        Some(EffectRecoveryAction::Probe) => {
                            self.hub
                                .probe_effect_outcome(
                                    recovery_session,
                                    effect.clone(),
                                    menu.id.clone(),
                                    envelope,
                                )
                                .await
                        }
                        Some(EffectRecoveryAction::Retry) => {
                            self.hub
                                .submit_effect_retry(
                                    recovery_session,
                                    effect.clone(),
                                    menu.id.clone(),
                                    envelope.seq,
                                )
                                .await
                        }
                        _ => Ok(()),
                    };
                    if let Err(error) = follow_up {
                        return self.menu_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            &error.message,
                            error.retryable,
                            None,
                        );
                    }
                }
                self.menu_success(request_id, envelope.seq)
            }
            Ok(MenuResolutionOutcome::IdempotentReplay { resolution_seq }) => {
                self.menu_success(request_id, resolution_seq)
            }
            Ok(MenuResolutionOutcome::AlreadyResolved { resolution_seq }) => self.menu_error(
                request_id,
                ERROR_CODE_ALREADY_RESOLVED,
                "menu was already resolved",
                false,
                Some(ErrorData::AlreadyResolved { resolution_seq }),
            ),
            Err(error) => {
                let code = match error.code {
                    ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
                    ErrorCode::MenuAlreadyAnswered => ERROR_CODE_ALREADY_RESOLVED,
                    ErrorCode::MenuNotFound | ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
                    _ => ERROR_CODE_INVALID_ARGUMENT,
                };
                self.menu_error(request_id, code, &error.message, error.retryable, None)
            }
        }
    }

    fn menu_success(
        &self,
        request_id: Option<RequestId>,
        resolution_seq: u64,
    ) -> Result<(), SessionHubError> {
        match request_id {
            Some(request_id) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::MenuAnswer { resolution_seq },
            }),
            None => Ok(()),
        }
    }

    fn menu_error(
        &self,
        request_id: Option<RequestId>,
        code: &str,
        message: &str,
        retryable: bool,
        data: Option<ErrorData>,
    ) -> Result<(), SessionHubError> {
        match request_id {
            Some(request_id) => self.respond_error(request_id, code, message, retryable, data),
            None => self.send(WireFrame::ProtocolError(ProtocolError {
                code: code.into(),
                message: message.into(),
                fatal: false,
                presentation: None,
                failed_write_ids: Vec::new(),
            })),
        }
    }

    fn respond_error(
        &self,
        request_id: RequestId,
        code: &str,
        message: &str,
        retryable: bool,
        data: Option<ErrorData>,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                code: code.into(),
                message: message.into(),
                retryable,
                data,
            },
        })
    }

    fn send(&self, frame: WireFrame) -> Result<(), SessionHubError> {
        self.sink
            .try_send(frame)
            .map_err(|_| SessionHubError::Delivery)
    }

    /// Detaches every attachment owned by this connection and wipes every
    /// staged secret (R7: disconnect wipes all staged secrets; a secret a
    /// login command already claimed lives on with the command).
    pub async fn close(&self) -> Result<(), SessionHubError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Ok(mut stages) = self.stages.lock() {
            *stages = crate::accounts::StagedSecrets::default();
        }
        if let Ok(Some(facade)) = self.hub.accounts()
            && let Some(oauth) = facade.oauth
        {
            oauth.cancel_connection(&self.connection_id);
        }
        self.hub.detach_connection(&self.connection_id).await
    }
}

fn standard_base64_decoded_len(encoded: &str) -> Result<usize, &'static str> {
    let bytes = encoded.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("artifact.put data_base64 must use padded RFC 4648 encoding");
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2
        || bytes[..bytes.len().saturating_sub(padding)]
            .iter()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(*byte, b'+' | b'/')))
        || bytes[..bytes.len().saturating_sub(padding)].contains(&b'=')
    {
        return Err("artifact.put data_base64 is not standard RFC 4648 base64");
    }
    Ok(bytes.len() / 4 * 3 - padding)
}

/// The ONE title-normalization seam (G2): control characters stripped,
/// trimmed, capped at 80 characters, empty collapses to `None`. The store
/// transaction re-asserts these bounds.
fn normalize_session_title(title: Option<String>) -> Option<String> {
    let cleaned: String = title?.chars().filter(|c| !c.is_control()).collect();
    let capped: String = cleaned.trim().chars().take(80).collect();
    let capped = capped.trim_end();
    if capped.is_empty() {
        None
    } else {
        Some(capped.to_owned())
    }
}

/// Daemon-side mirror of the TUI's `slug_name` (G2 auto-title): first three
/// whitespace words, joined by `-`, lowercased, `[a-z0-9-]` only, at most
/// 28 characters, fallback `session`.
fn auto_title_slug(text: &str) -> String {
    let joined = text
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    let slug: String = joined
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(28)
        .collect();
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    }
}

/// The capability→delivery JOIN: the single decision that turns a session
/// provider's PDF capability into the delivery mode journaled on the user
/// message. Pinned directly (wd_pdf_runtime_tests) because the capability
/// table and the downstream shaping are each pinned yet an inverted join
/// survived both.
pub(crate) fn pdf_delivery_for_provider(provider: &str) -> haider_protocol::tool::PdfDeliveryMode {
    if haider_provider::pdf_document_capability(provider)
        == haider_protocol::provider::FeatureResolve::Native
    {
        haider_protocol::tool::PdfDeliveryMode::NativeDocument
    } else {
        haider_protocol::tool::PdfDeliveryMode::ExtractedText
    }
}

async fn validate_turn_attachments(
    store: &haider_core::SqliteStoreHandle,
    attachments: &[haider_protocol::tool::AttachmentBlock],
    pdf_delivery: haider_protocol::tool::PdfDeliveryMode,
) -> Result<Vec<haider_protocol::tool::AttachmentBlock>, AttachmentValidationFailure> {
    if attachments.len() > MAX_ATTACHMENTS_PER_TURN {
        let actual_count = u32::try_from(attachments.len()).unwrap_or(u32::MAX);
        return Err(AttachmentValidationFailure {
            code: ERROR_CODE_TOO_MANY_ATTACHMENTS,
            message: format!(
                "turn carries {actual_count} attachments; the limit is {MAX_ATTACHMENTS_PER_TURN}"
            ),
            data: Some(ErrorData::TooManyAttachments {
                actual_count,
                max_count: MAX_ATTACHMENTS_PER_TURN as u32,
            }),
        });
    }

    let mut total_bytes = 0_usize;
    let aggregate_limit = if attachments.iter().any(|attachment| {
        matches!(
            attachment,
            haider_protocol::tool::AttachmentBlock::Pdf { .. }
        )
    }) {
        // PDF is the only attachment lane whose typed per-file budget is
        // larger than the historical 16 MiB turn aggregate. Keep the legacy
        // law exact for every pre-PDF turn.
        MAX_ATTACHMENT_BYTES_PER_PDF_TURN
    } else {
        MAX_ATTACHMENT_BYTES_PER_TURN
    };
    let mut canonical = Vec::with_capacity(attachments.len());
    for (index, attachment) in attachments.iter().enumerate() {
        let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
        let artifact = match attachment {
            haider_protocol::tool::AttachmentBlock::Image { artifact, mime, .. } => {
                if !IMAGE_ATTACHMENT_MIME_ALLOWLIST.contains(&mime.as_str()) {
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED,
                        message: format!(
                            "attachment {index} declares unsupported image MIME `{mime}`; use image/jpeg, image/png, image/gif, or image/webp"
                        ),
                        data: Some(ErrorData::AttachmentMimeUnsupported {
                            index: index_u32,
                            mime: mime.clone(),
                        }),
                    });
                }
                artifact
            }
            haider_protocol::tool::AttachmentBlock::PastedText { artifact, .. } => artifact,
            haider_protocol::tool::AttachmentBlock::File { artifact, name, .. } => {
                // Name sanity (G2): a display basename, never a path and
                // never terminal-control bytes. The cap mirrors the client
                // loader; violation is a client bug, refused honestly.
                if name.is_empty()
                    || name.chars().count() > 120
                    || name.chars().any(char::is_control)
                    || name.contains('/')
                    || name.contains('\\')
                {
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_INVALID_ARGUMENT,
                        message: format!(
                            "attachment {index} declares an invalid file name; names are non-empty basenames of at most 120 characters with no control characters"
                        ),
                        data: None,
                    });
                }
                artifact
            }
            haider_protocol::tool::AttachmentBlock::Pdf { artifact, name, .. } => {
                if name.is_empty()
                    || name.chars().count() > 120
                    || name.chars().any(char::is_control)
                    || name.contains('/')
                    || name.contains('\\')
                {
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_INVALID_ARGUMENT,
                        message: format!(
                            "attachment {index} declares an invalid PDF name; names are non-empty basenames of at most 120 characters with no control characters"
                        ),
                        data: None,
                    });
                }
                artifact
            }
            haider_protocol::tool::AttachmentBlock::Skill { name, .. } => {
                return Err(AttachmentValidationFailure {
                    code: ERROR_CODE_INVALID_ARGUMENT,
                    message: format!("skill attachment `{name}` is reserved but not yet supported"),
                    data: None,
                });
            }
        };
        let bytes = store.get(artifact).await.map_err(|_| AttachmentValidationFailure {
            code: ERROR_CODE_ATTACHMENT_NOT_FOUND,
            message: format!(
                "attachment {index} references unavailable or unverified artifact {artifact}; upload it with artifact.put and retry"
            ),
            data: Some(ErrorData::AttachmentNotFound {
                index: index_u32,
                artifact: artifact.clone(),
            }),
        })?;
        let canonical_attachment = match attachment {
            haider_protocol::tool::AttachmentBlock::Pdf { name, .. } => {
                if bytes.len() > haider_pdf::MAX_PDF_BYTES {
                    let actual_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                    let message = format!(
                        "PDF attachment {index} is {actual_bytes} bytes; the PDF limit is {}",
                        haider_pdf::MAX_PDF_BYTES
                    );
                    let presentation = haider_protocol::error::ErrorPresentation::new(
                        "pdf-too-large",
                        "PDF is too large",
                        &message,
                        haider_protocol::error::ErrorScope::Turn,
                        [haider_protocol::error::ErrorAction::None],
                    );
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_PDF_TOO_LARGE,
                        message,
                        data: Some(ErrorData::PdfTooLarge {
                            index: index_u32,
                            artifact: artifact.clone(),
                            actual_bytes,
                            max_bytes: haider_pdf::MAX_PDF_BYTES as u64,
                            presentation,
                        }),
                    });
                }
                let metadata = haider_pdf::inspect_pdf(&bytes).map_err(|error| {
                    let message = format!("PDF attachment {index} could not be parsed: {error}");
                    AttachmentValidationFailure {
                        code: ERROR_CODE_PDF_MALFORMED,
                        data: Some(ErrorData::PdfMalformed {
                            index: index_u32,
                            artifact: artifact.clone(),
                            presentation: haider_protocol::error::ErrorPresentation::new(
                                "pdf-malformed",
                                "PDF could not be read",
                                &message,
                                haider_protocol::error::ErrorScope::Turn,
                                [haider_protocol::error::ErrorAction::None],
                            ),
                        }),
                        message,
                    }
                })?;
                if metadata.pages > haider_pdf::MAX_PDF_PAGES {
                    let message = format!(
                        "PDF attachment {index} has {} pages; the limit is {} pages",
                        metadata.pages,
                        haider_pdf::MAX_PDF_PAGES
                    );
                    let presentation = haider_protocol::error::ErrorPresentation::new(
                        "pdf-too-many-pages",
                        "PDF has too many pages",
                        &message,
                        haider_protocol::error::ErrorScope::Turn,
                        [haider_protocol::error::ErrorAction::None],
                    );
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_PDF_TOO_MANY_PAGES,
                        message,
                        data: Some(ErrorData::PdfTooManyPages {
                            index: index_u32,
                            artifact: artifact.clone(),
                            actual_pages: metadata.pages,
                            max_pages: haider_pdf::MAX_PDF_PAGES,
                            presentation,
                        }),
                    });
                }
                haider_protocol::tool::AttachmentBlock::Pdf {
                    artifact: artifact.clone(),
                    name: name.clone(),
                    pages: metadata.pages,
                    delivery: pdf_delivery,
                }
            }
            haider_protocol::tool::AttachmentBlock::File { .. } => {
                if bytes.len() > MAX_ATTACHMENT_BYTES {
                    return Err(oversized_attachment(index_u32, artifact, bytes.len()));
                }
                if std::str::from_utf8(&bytes).is_err() {
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_INVALID_ARGUMENT,
                        message: format!(
                            "attachment {index} is not UTF-8 text; only UTF-8 text files can be attached (unsupported_attachment_encoding)"
                        ),
                        data: None,
                    });
                }
                attachment.clone()
            }
            _ => {
                if bytes.len() > MAX_ATTACHMENT_BYTES {
                    return Err(oversized_attachment(index_u32, artifact, bytes.len()));
                }
                attachment.clone()
            }
        };
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > aggregate_limit {
            let actual_bytes = u64::try_from(total_bytes).unwrap_or(u64::MAX);
            return Err(AttachmentValidationFailure {
                code: ERROR_CODE_ATTACHMENTS_TOO_LARGE,
                message: format!(
                    "turn attachments total {actual_bytes} bytes; the aggregate limit is {aggregate_limit}"
                ),
                data: Some(ErrorData::AttachmentsTooLarge {
                    actual_bytes,
                    max_bytes: aggregate_limit as u64,
                }),
            });
        }
        canonical.push(canonical_attachment);
    }
    Ok(canonical)
}

fn oversized_attachment(
    index: u32,
    artifact: &haider_protocol::ids::ArtifactRef,
    bytes: usize,
) -> AttachmentValidationFailure {
    let actual_bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    AttachmentValidationFailure {
        code: ERROR_CODE_ATTACHMENT_TOO_LARGE,
        message: format!(
            "attachment {index} is {actual_bytes} bytes; the per-attachment limit is {MAX_ATTACHMENT_BYTES}"
        ),
        data: Some(ErrorData::AttachmentTooLarge {
            index,
            artifact: artifact.clone(),
            actual_bytes,
            max_bytes: MAX_ATTACHMENT_BYTES as u64,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    View,
    Control,
}

fn authorize(capabilities: &CapabilitySet, operation: Operation) -> Result<(), &'static str> {
    let allowed = match operation {
        Operation::View => {
            capabilities.contains(&Capability::View) || capabilities.contains(&Capability::Control)
        }
        Operation::Control => capabilities.contains(&Capability::Control),
    };
    allowed.then_some(()).ok_or(match operation {
        Operation::View => "this method requires the view capability",
        Operation::Control => "this method requires the control capability",
    })
}

fn valid_device_candidate_id(candidate: &str) -> bool {
    candidate.len() == 68
        && candidate.starts_with("dc1_")
        && candidate.as_bytes()[4..].iter().all(u8::is_ascii_hexdigit)
}

struct ValidatedWorkspace {
    canonical: String,
    descriptor: std::fs::File,
}

async fn validate_workspace(cwd: String) -> Result<ValidatedWorkspace, String> {
    if !std::path::Path::new(&cwd).is_absolute() {
        return Err("session cwd must be an absolute path".into());
    }
    tokio::task::spawn_blocking(move || {
        let canonical = std::fs::canonicalize(&cwd)
            .map_err(|error| format!("cannot canonicalize session cwd: {error}"))?;
        let canonical_text = canonical
            .to_str()
            .ok_or_else(|| "canonical session cwd is not valid UTF-8".to_owned())?
            .to_owned();
        let metadata = std::fs::metadata(&canonical)
            .map_err(|error| format!("cannot inspect session cwd: {error}"))?;
        if !metadata.is_dir() {
            return Err("session cwd must identify a directory".into());
        }
        let descriptor = std::fs::File::open(&canonical)
            .map_err(|error| format!("cannot open session cwd: {error}"))?;
        Ok(ValidatedWorkspace {
            canonical: canonical_text,
            descriptor,
        })
    })
    .await
    .map_err(|error| format!("session cwd validation task failed: {error}"))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod error_wave3_tests {
    use super::*;

    #[tokio::test]
    async fn e6a_probe_reexecutes_the_filesystem_check() {
        let root = tempfile::tempdir().expect("workspace");
        let target = root.path().join("outcome.txt");
        std::fs::write(&target, b"first").expect("first write");
        let intent = EffectIntent {
            effect: haider_protocol::ids::EffectId::new("effect-e6a-probe"),
            class: EffectClass::FsWrite,
            summary: "write outcome.txt".into(),
            args_digest: "args-e6a".into(),
            workspace_revision: None,
        };

        let first =
            effect_probe_observation(&intent, root.path().to_str().expect("utf8 workspace"), None)
                .await;
        std::fs::write(&target, b"second content").expect("second write");
        let second =
            effect_probe_observation(&intent, root.path().to_str().expect("utf8 workspace"), None)
                .await;

        assert!(first.contains("5 bytes"), "{first}");
        assert!(second.contains("14 bytes"), "{second}");
        assert_ne!(first, second, "probe must inspect current state each time");
    }
}
