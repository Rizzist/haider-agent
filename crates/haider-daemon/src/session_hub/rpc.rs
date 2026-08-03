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
use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::agent::ChipState;
use haider_protocol::context::ContextFootprint;
use haider_protocol::item::ItemEvent;
use haider_protocol::menu::MenuKind;
use haider_protocol::state::RunState;
use std::collections::{BTreeMap, VecDeque};

const MAX_ATTACHMENTS_PER_TURN: usize = 5;
const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES_PER_TURN: usize = 16 * 1024 * 1024;

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
    let mut since_seq = 0;
    let mut latest = None;
    while since_seq < through_seq {
        let page = store.read(session_id, since_seq, REPLAY_PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                return Ok(latest);
            }
            since_seq = envelope.seq;
            advanced = true;
            let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            if let Some(footprint) = ContextFootprint::from_extension_item(&item) {
                latest = Some(footprint);
            }
        }
        if !advanced {
            break;
        }
    }
    Ok(latest)
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
                let permission_description = match &menu.kind {
                    MenuKind::Permission { effect_summary } => Some(effect_summary.clone()),
                    _ => None,
                };
                self.menus.insert(
                    menu.id.as_str().to_owned(),
                    haider_rpc::ObserveMenuWire {
                        kind: observe_menu_kind(&menu.kind).into(),
                        title: menu.title,
                        permission_description,
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
        MenuKind::Exhausted => "exhausted",
        MenuKind::TrustHook => "trust_hook",
        MenuKind::Update => "update",
        MenuKind::Question => "question",
        MenuKind::Choice => "choice",
        MenuKind::Secret => "secret",
        MenuKind::File => "file",
        MenuKind::Conflict => "conflict",
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
                    request_id, command_id, cwd, provider, model, max_tokens, None,
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
            RequestBody::AccountSetActive { command_id, alias } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_set_active(request_id, command_id, alias)
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
            || !matches!(source.as_str(), "codex" | "claude-code")
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.oauth_import requires a command id and source `codex` or `claude-code`",
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

    fn account_set_active(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        alias: String,
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
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "branch_id": &branch_id,
            "text": &text,
            "attachments": &attachments,
            "mode": mode,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode turn-submit coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .turn_accept_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(accepted)) => {
                if accepted.worker_generation == self.hub.inner.store.worker_generation()
                    && let Err(error) = self.hub.worker_manager()?.submit(accepted.clone()).await
                {
                    return self.respond_turn_error(request_id, error);
                }
                return self.respond_turn_accepted(request_id, accepted);
            }
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        match validate_turn_attachments(&self.hub.inner.store, &attachments).await {
            Ok(()) => {}
            Err(failure) => {
                return self.respond_error(
                    request_id,
                    failure.code,
                    &failure.message,
                    false,
                    failure.data,
                );
            }
        }
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
        // Durable-before-provider: the manager sees this only after the actor
        // committed and synchronously published the acceptance transaction.
        if let Err(error) = self.hub.worker_manager()?.submit(accepted.clone()).await {
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
            sessions.push(SessionSummary {
                session_id: session_id.clone(),
                head_seq: self.hub.inner.store.latest_seq(session_id).await?,
                worker_generation: self.hub.inner.store.worker_generation(),
                metadata: self.hub.inner.store.session_metadata(session_id).await?,
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
            Ok(MenuResolutionOutcome::Committed { ref envelope }) => {
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

async fn validate_turn_attachments(
    store: &haider_core::SqliteStoreHandle,
    attachments: &[haider_protocol::tool::AttachmentBlock],
) -> Result<(), AttachmentValidationFailure> {
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
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            let actual_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            return Err(AttachmentValidationFailure {
                code: ERROR_CODE_ATTACHMENT_TOO_LARGE,
                message: format!(
                    "attachment {index} is {actual_bytes} bytes; the per-attachment limit is {MAX_ATTACHMENT_BYTES}"
                ),
                data: Some(ErrorData::AttachmentTooLarge {
                    index: index_u32,
                    artifact: artifact.clone(),
                    actual_bytes,
                    max_bytes: MAX_ATTACHMENT_BYTES as u64,
                }),
            });
        }
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_ATTACHMENT_BYTES_PER_TURN {
            let actual_bytes = u64::try_from(total_bytes).unwrap_or(u64::MAX);
            return Err(AttachmentValidationFailure {
                code: ERROR_CODE_ATTACHMENTS_TOO_LARGE,
                message: format!(
                    "turn attachments total {actual_bytes} bytes; the aggregate limit is {MAX_ATTACHMENT_BYTES_PER_TURN}"
                ),
                data: Some(ErrorData::AttachmentsTooLarge {
                    actual_bytes,
                    max_bytes: MAX_ATTACHMENT_BYTES_PER_TURN as u64,
                }),
            });
        }
    }
    Ok(())
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
