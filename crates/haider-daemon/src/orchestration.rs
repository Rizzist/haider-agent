//! Daemon-owned admission and deterministic state for `tool_script`.
//!
//! Tool dispatch deliberately remains in `worker`: that is the one place that
//! owns the existing effect broker. This module only admits immutable pipe
//! rows, verifies ref closures, and evaluates pure definitions.

use crate::session_hub::HubStoreHandle;
use haider_core::StoreHandle;
use haider_protocol::EventPayload;
use haider_protocol::ids::ArtifactRef;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::orchestration::{
    InlineNodeV1, InlinePortV1, MaterializedDefinitionV1, MaterializedPortV1, ORCHESTRATION_CODEC,
    ORCHESTRATION_NODE_MAX, ORCHESTRATION_SCRIPT_EXTENSION, ORCHESTRATION_SEMANTICS,
    ORCHESTRATION_TRANSPORT, ORCHESTRATION_VERSION, OrchIndexDomainV1, OrchIndexEntryV1,
    OrchIndexV1, OrchPortRoleV1, OrchShapeV1, OrchTypeV1, OrchestrationError,
    OrchestrationLimitsV1, ReadGroupV1, ScriptGraphV1, ScriptRequestV1, ScriptTerminalV1,
    StrictJson, canonical_json, parse_script_request, request_digest, validate_inline_dag,
};
use haider_protocol::pipe::{INSTRUCT_EVIDENCE_MAX_BYTES, InstructEvidenceRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

const REF_SCAN_PAGE: usize = 1_024;
const REF_SCAN_BYTES: usize = 4 * 1_024 * 1_024;

#[derive(Debug, Clone)]
pub(crate) struct WrapperSnapshot {
    pub(crate) catalog_digest: String,
    pub(crate) wrappers: HashMap<String, WrapperV1>,
}

#[derive(Debug, Clone)]
pub(crate) struct WrapperV1 {
    pub(crate) digest: String,
    pub(crate) repeat_safe: bool,
    pub(crate) parallel_safe: bool,
    pub(crate) effectless_actor: bool,
    pub(crate) may_ask: bool,
    pub(crate) input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmittedScriptV1 {
    pub(crate) version: u32,
    pub(crate) script_id: String,
    pub(crate) request_digest: String,
    pub(crate) shape_ref: InstructEvidenceRef,
    pub(crate) source_ref: InstructEvidenceRef,
    pub(crate) definition_refs: Vec<InstructEvidenceRef>,
    pub(crate) parameters: Vec<OrchTypeV1>,
    pub(crate) nodes: Vec<InlineNodeV1>,
    pub(crate) exits: Vec<u32>,
    pub(crate) read_groups: Vec<ReadGroupV1>,
    pub(crate) inputs: Vec<StrictJson>,
    pub(crate) limits: OrchestrationLimitsV1,
    pub(crate) admitted_at_ms: u64,
    pub(crate) canonical_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RuntimeValueV1 {
    Ready { value: StrictJson },
    Inactive,
}

impl RuntimeValueV1 {
    pub(crate) fn value(&self) -> Option<&Value> {
        match self {
            Self::Ready { value } => Some(&value.0),
            Self::Inactive => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingCallV1 {
    pub(crate) slot: u32,
    pub(crate) attempt: u8,
    pub(crate) item_id: String,
    pub(crate) call_id: String,
    pub(crate) tool: String,
    pub(crate) args: StrictJson,
    pub(crate) activation_ref: InstructEvidenceRef,
    pub(crate) started_at_ms: u64,
    pub(crate) deadline_ms: u64,
    pub(crate) started: bool,
    #[serde(default)]
    pub(crate) approval_waiting: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CallClockV1 {
    pub(crate) started_at_ms: u64,
    pub(crate) deadline_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retry_not_before_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeStateV1 {
    pub(crate) admitted: AdmittedScriptV1,
    pub(crate) values: Vec<Option<RuntimeValueV1>>,
    pub(crate) value_refs: Vec<Option<InstructEvidenceRef>>,
    pub(crate) next_slot: u32,
    pub(crate) attempts: BTreeMap<u32, u8>,
    pub(crate) call_clocks: BTreeMap<u32, CallClockV1>,
    pub(crate) pending_calls: BTreeMap<u32, PendingCallV1>,
    pub(crate) receipt_refs: Vec<InstructEvidenceRef>,
    pub(crate) checkpoint_ref: Option<InstructEvidenceRef>,
    pub(crate) completed_calls: u32,
    pub(crate) failed_calls: u32,
    pub(crate) rejected_calls: u32,
    pub(crate) unknown_calls: u32,
    #[serde(default)]
    pub(crate) retained_screenshot_count: u32,
    #[serde(default)]
    pub(crate) retained_screenshot_bytes: u64,
    #[serde(default)]
    pub(crate) interaction_observation_invalidated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableCheckpointV1 {
    version: u32,
    script_id: String,
    request_digest: String,
    source_ref: InstructEvidenceRef,
    admitted_at_ms: u64,
    canonical_bytes: u64,
    value_indexes: Vec<InstructEvidenceRef>,
    inactive_slots: Vec<u32>,
    next_slot: u32,
    attempts: BTreeMap<u32, u8>,
    call_clocks: BTreeMap<u32, CallClockV1>,
    pending_calls: BTreeMap<u32, PendingCallV1>,
    receipt_refs: Vec<InstructEvidenceRef>,
    checkpoint_ref: Option<InstructEvidenceRef>,
    completed_calls: u32,
    failed_calls: u32,
    rejected_calls: u32,
    unknown_calls: u32,
    #[serde(default)]
    retained_screenshot_count: u32,
    #[serde(default)]
    retained_screenshot_bytes: u64,
    #[serde(default)]
    interaction_observation_invalidated: bool,
}

#[derive(Debug)]
pub(crate) struct AdmissionFailure {
    pub(crate) request_digest: String,
    pub(crate) code: String,
    pub(crate) message: String,
}

impl From<OrchestrationError> for AdmissionFailure {
    fn from(error: OrchestrationError) -> Self {
        Self {
            request_digest: String::new(),
            code: error.code.into(),
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptManifestV1 {
    version: u32,
    transport: String,
    codec: String,
    semantics: String,
    catalog_digest: String,
    shape: InstructEvidenceRef,
    inputs: Vec<InstructEvidenceRef>,
    limits: haider_protocol::orchestration::OrchestrationLimitsV1,
}

pub(crate) async fn admit(
    raw: &[u8],
    wrappers: &WrapperSnapshot,
    store: &HubStoreHandle,
    script_id: String,
    admitted_at_ms: u64,
) -> Result<RuntimeStateV1, AdmissionFailure> {
    let digest = request_digest(raw);
    let request = parse_script_request(raw).map_err(|error| AdmissionFailure {
        request_digest: digest.clone(),
        code: error.code.into(),
        message: error.to_string(),
    })?;
    if request.catalog_digest != wrappers.catalog_digest {
        return Err(AdmissionFailure {
            request_digest: digest,
            code: "stale_catalog".into(),
            message: "tool_script catalog_digest does not match the entry catalog".into(),
        });
    }
    let (parameters, nodes, exits, read_groups, definition_refs, shape_ref, canonical_bytes) =
        match &request.graph {
            ScriptGraphV1::Inline {
                parameters,
                nodes,
                exits,
                read_groups,
            } => {
                let mut nodes = nodes.clone();
                normalize_node_configs(&mut nodes);
                validate_inline_dag(
                    parameters,
                    &nodes,
                    exits,
                    read_groups,
                    request.limits.as_ref(),
                )
                .map_err(|error| AdmissionFailure {
                    request_digest: digest.clone(),
                    code: error.code.into(),
                    message: error.to_string(),
                })?;
                validate_wrappers(&nodes, wrappers, &digest)?;
                validate_read_group_wrappers(&nodes, read_groups, wrappers, &digest)?;
                let materialized =
                    materialize_inline(parameters, &nodes, exits, read_groups, &request, store)
                        .await
                        .map_err(|error| admission_store_failure(&digest, error))?;
                (
                    parameters.clone(),
                    nodes,
                    exits.clone(),
                    read_groups.clone(),
                    materialized.definition_refs,
                    materialized.shape_ref,
                    materialized.canonical_bytes,
                )
            }
            ScriptGraphV1::Ref { root } => {
                if !session_has_shape_access(store, root)
                    .await
                    .map_err(|error| admission_store_failure(&digest, error))?
                {
                    return Err(AdmissionFailure {
                        request_digest: digest,
                        code: "ref_access_denied".into(),
                        message: "the session has no committed access to this shape ref".into(),
                    });
                }
                let imported = import_shape(root, &request.catalog_digest, store, &digest).await?;
                let mut normalized = imported.nodes.clone();
                normalize_node_configs(&mut normalized);
                if normalized != imported.nodes {
                    return Err(AdmissionFailure {
                        request_digest: digest,
                        code: "non_normalized_config".into(),
                        message: "referenced definitions omit canonical default config fields"
                            .into(),
                    });
                }
                validate_inline_dag(
                    &imported.parameters,
                    &imported.nodes,
                    &imported.exits,
                    &imported.read_groups,
                    request.limits.as_ref(),
                )
                .map_err(|error| AdmissionFailure {
                    request_digest: digest.clone(),
                    code: error.code.into(),
                    message: error.to_string(),
                })?;
                validate_wrappers(&imported.nodes, wrappers, &digest)?;
                validate_read_group_wrappers(
                    &imported.nodes,
                    &imported.read_groups,
                    wrappers,
                    &digest,
                )?;
                (
                    imported.parameters,
                    imported.nodes,
                    imported.exits,
                    imported.read_groups,
                    imported.definition_refs,
                    root.clone(),
                    imported.canonical_bytes,
                )
            }
        };
    if request.inputs.len() != parameters.len() {
        return Err(AdmissionFailure {
            request_digest: digest,
            code: "input_count".into(),
            message: "inputs must match the declared parameter count".into(),
        });
    }
    for (value, parameter) in request.inputs.iter().zip(&parameters) {
        validate_value(&value.0, parameter, 0).map_err(|message| AdmissionFailure {
            request_digest: digest.clone(),
            code: "input_type".into(),
            message,
        })?;
    }
    let limits = effective_limits(request.limits.clone());
    validate_preflight_arguments(&nodes, &request.inputs, wrappers, &digest)?;
    validate_interaction_sequences(&nodes, &request.inputs, &limits, &digest)?;
    let mut input_refs = Vec::with_capacity(request.inputs.len());
    let mut total_bytes = canonical_bytes;
    for (ordinal, (value, value_type)) in request.inputs.iter().zip(&parameters).enumerate() {
        let payload = serde_json::json!({
            "version": 1,
            "kind": "input",
            "ordinal": ordinal,
            "type": value_type,
            "value": value,
        });
        let (evidence_ref, bytes) = put_evidence(store, "OrchResultV1", &payload, vec![])
            .await
            .map_err(|error| admission_store_failure(&digest, error))?;
        total_bytes = total_bytes.saturating_add(bytes);
        input_refs.push(evidence_ref);
    }
    let manifest = ScriptManifestV1 {
        version: ORCHESTRATION_VERSION,
        transport: ORCHESTRATION_TRANSPORT.into(),
        codec: ORCHESTRATION_CODEC.into(),
        semantics: ORCHESTRATION_SEMANTICS.into(),
        catalog_digest: request.catalog_digest.clone(),
        shape: shape_ref.clone(),
        inputs: input_refs.clone(),
        limits: limits.clone(),
    };
    let mut parents = Vec::with_capacity(input_refs.len() + 1);
    parents.push(shape_ref.artifact.clone());
    parents.extend(input_refs.iter().map(|value| value.artifact.clone()));
    let (source_ref, bytes) = put_evidence(store, "OrchScriptV1", &manifest, parents)
        .await
        .map_err(|error| admission_store_failure(&digest, error))?;
    total_bytes = total_bytes.saturating_add(bytes);
    Ok(RuntimeStateV1 {
        values: vec![None; nodes.len()],
        value_refs: vec![None; nodes.len()],
        next_slot: 0,
        attempts: BTreeMap::new(),
        call_clocks: BTreeMap::new(),
        pending_calls: BTreeMap::new(),
        receipt_refs: Vec::new(),
        checkpoint_ref: None,
        completed_calls: 0,
        failed_calls: 0,
        rejected_calls: 0,
        unknown_calls: 0,
        retained_screenshot_count: 0,
        retained_screenshot_bytes: 0,
        interaction_observation_invalidated: false,
        admitted: AdmittedScriptV1 {
            version: ORCHESTRATION_VERSION,
            script_id,
            request_digest: digest,
            shape_ref,
            source_ref,
            definition_refs,
            parameters,
            nodes,
            exits,
            read_groups,
            inputs: request.inputs,
            limits,
            admitted_at_ms,
            canonical_bytes: total_bytes,
        },
    })
}

fn normalize_node_configs(nodes: &mut [InlineNodeV1]) {
    for node in nodes {
        let Some(config) = node.config.0.as_object_mut() else {
            continue;
        };
        match node.evidence_type.as_str() {
            "OrchValueV1" | "OrchArgumentV1" | "OrchAskPauseV1" | "OrchCallV1" | "OrchAwaitV1"
            | "OrchBranchV1" | "OrchJoinV1" | "OrchExitV1" => {
                config
                    .entry("region")
                    .or_insert_with(|| Value::Array(Vec::new()));
            }
            _ => {}
        }
        match node.evidence_type.as_str() {
            "OrchRetryPolicyV1" => {
                config
                    .entry("max_attempts")
                    .or_insert_with(|| Value::from(1));
            }
            "OrchAskPauseV1" => {
                config
                    .entry("wait_ms")
                    .or_insert_with(|| Value::from(120_000));
            }
            "OrchAwaitV1" => {
                config
                    .entry("on_error")
                    .or_insert_with(|| Value::String("stop".into()));
            }
            "OrchJoinV1" => {
                config
                    .entry("omit_inactive")
                    .or_insert_with(|| Value::Bool(false));
            }
            _ => {}
        }
    }
}

struct Materialized {
    definition_refs: Vec<InstructEvidenceRef>,
    shape_ref: InstructEvidenceRef,
    canonical_bytes: u64,
}

async fn materialize_inline(
    parameters: &[OrchTypeV1],
    nodes: &[InlineNodeV1],
    exits: &[u32],
    read_groups: &[ReadGroupV1],
    request: &ScriptRequestV1,
    store: &HubStoreHandle,
) -> Result<Materialized, haider_protocol::error::HaiderError> {
    let mut refs: Vec<InstructEvidenceRef> = Vec::with_capacity(nodes.len());
    let mut total = 0u64;
    for node in nodes {
        let ports = node
            .ports
            .iter()
            .map(|port| {
                let parent = &refs[port.source_slot as usize];
                MaterializedPortV1 {
                    role: port.role,
                    port: port.port.clone(),
                    parent: parent.artifact.clone(),
                    parent_ledger_digest: parent.ledger_digest.clone(),
                    output: port.output.clone(),
                    selector: port.selector.clone(),
                }
            })
            .collect::<Vec<_>>();
        let definition = MaterializedDefinitionV1 {
            version: ORCHESTRATION_VERSION,
            evidence_type: node.evidence_type.clone(),
            slot: node.slot,
            config: node.config.clone(),
            ports,
        };
        let parents = definition
            .ports
            .iter()
            .map(|port| port.parent.clone())
            .collect();
        let (evidence_ref, bytes) =
            put_evidence(store, &node.evidence_type, &definition, parents).await?;
        total = total.saturating_add(bytes);
        refs.push(evidence_ref);
    }
    let mut indexes = Vec::new();
    for page in refs.chunks(256) {
        let entries = page
            .iter()
            .enumerate()
            .map(|(index, evidence_ref)| OrchIndexEntryV1 {
                slot: u32::try_from(indexes.len() * 256 + index).unwrap_or(u32::MAX),
                evidence_ref: evidence_ref.clone(),
            })
            .collect::<Vec<_>>();
        let index = OrchIndexV1 {
            version: ORCHESTRATION_VERSION,
            domain: OrchIndexDomainV1::Definition,
            script_id: None,
            entries,
        };
        let parents = page.iter().map(|entry| entry.artifact.clone()).collect();
        let (evidence_ref, bytes) = put_evidence(store, "OrchIndexV1", &index, parents).await?;
        total = total.saturating_add(bytes);
        indexes.push(evidence_ref);
    }
    let shape = OrchShapeV1 {
        version: ORCHESTRATION_VERSION,
        transport: ORCHESTRATION_TRANSPORT.into(),
        codec: ORCHESTRATION_CODEC.into(),
        semantics: ORCHESTRATION_SEMANTICS.into(),
        catalog_digest: request.catalog_digest.clone(),
        parameters: parameters.to_vec(),
        definition_count: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
        indexes: indexes.clone(),
        exits: exits.to_vec(),
        read_groups: read_groups.to_vec(),
    };
    let parents = indexes.iter().map(|index| index.artifact.clone()).collect();
    let (shape_ref, bytes) = put_evidence(store, "OrchShapeV1", &shape, parents).await?;
    total = total.saturating_add(bytes);
    Ok(Materialized {
        definition_refs: refs,
        shape_ref,
        canonical_bytes: total,
    })
}

pub(crate) async fn put_evidence<T: Serialize>(
    store: &HubStoreHandle,
    evidence_type: &str,
    value: &T,
    parents: Vec<ArtifactRef>,
) -> Result<(InstructEvidenceRef, u64), haider_protocol::error::HaiderError> {
    let bytes = canonical_json(value).map_err(|error| {
        haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::InvalidArgument,
            error.to_string(),
            false,
        )
    })?;
    if bytes.is_empty() || bytes.len() as u64 > INSTRUCT_EVIDENCE_MAX_BYTES {
        return Err(haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::InvalidArgument,
            "orchestration evidence row exceeds 1 MiB",
            false,
        ));
    }
    let len = bytes.len() as u64;
    let artifact = store.put_artifact(bytes).await?;
    Ok((
        InstructEvidenceRef::new(artifact, evidence_type, len, parents),
        len,
    ))
}

pub(crate) async fn persist_runtime_value(
    store: &HubStoreHandle,
    state: &RuntimeStateV1,
    node: &InlineNodeV1,
    value: &RuntimeValueV1,
) -> Result<Option<InstructEvidenceRef>, haider_protocol::error::HaiderError> {
    let RuntimeValueV1::Ready { value } = value else {
        return Ok(None);
    };
    let kind = match node.evidence_type.as_str() {
        "OrchValueV1" => "value",
        "OrchArgumentV1" => "argument",
        "OrchRetryPolicyV1" => "retry",
        "OrchAskPauseV1" => "ask",
        "OrchAwaitV1" => "await",
        "OrchBranchV1" => "branch",
        "OrchJoinV1" => "join",
        "OrchExitV1" => "value",
        _ => "value",
    };
    let payload = serde_json::json!({
        "version": ORCHESTRATION_VERSION,
        "kind": kind,
        "script_id": state.admitted.script_id,
        "slot": node.slot,
        "value": value,
    });
    let mut parents = vec![
        state.admitted.source_ref.artifact.clone(),
        state.admitted.definition_refs[node.slot as usize]
            .artifact
            .clone(),
    ];
    parents.extend(node.ports.iter().filter_map(|port| {
        state
            .value_refs
            .get(port.source_slot as usize)
            .and_then(Option::as_ref)
            .map(|evidence| evidence.artifact.clone())
    }));
    put_evidence(store, "OrchResultV1", &payload, parents)
        .await
        .map(|(evidence_ref, _)| Some(evidence_ref))
}

pub(crate) async fn persist_checkpoint(
    store: &HubStoreHandle,
    state: &mut RuntimeStateV1,
) -> Result<InstructEvidenceRef, haider_protocol::error::HaiderError> {
    let mut ready = Vec::new();
    let mut inactive_slots = Vec::new();
    for (slot, (value, evidence_ref)) in state.values.iter().zip(&state.value_refs).enumerate() {
        match value {
            None => {}
            Some(RuntimeValueV1::Inactive) => {
                inactive_slots.push(u32::try_from(slot).unwrap_or(u32::MAX));
            }
            Some(RuntimeValueV1::Ready { .. }) => {
                let evidence_ref = evidence_ref.clone().ok_or_else(|| {
                    haider_protocol::error::HaiderError::new(
                        haider_protocol::error::ErrorCode::Internal,
                        "ready orchestration value has no immutable evidence ref",
                        false,
                    )
                })?;
                ready.push(OrchIndexEntryV1 {
                    slot: u32::try_from(slot).unwrap_or(u32::MAX),
                    evidence_ref,
                });
            }
        }
    }
    let mut value_indexes = Vec::new();
    for page in ready.chunks(256) {
        let index = OrchIndexV1 {
            version: ORCHESTRATION_VERSION,
            domain: OrchIndexDomainV1::Execution,
            script_id: Some(state.admitted.script_id.clone()),
            entries: page.to_vec(),
        };
        let parents = page
            .iter()
            .map(|entry| entry.evidence_ref.artifact.clone())
            .collect();
        let (index_ref, _) = put_evidence(store, "OrchIndexV1", &index, parents).await?;
        value_indexes.push(index_ref);
    }
    let durable = DurableCheckpointV1 {
        version: ORCHESTRATION_VERSION,
        script_id: state.admitted.script_id.clone(),
        request_digest: state.admitted.request_digest.clone(),
        source_ref: state.admitted.source_ref.clone(),
        admitted_at_ms: state.admitted.admitted_at_ms,
        canonical_bytes: state.admitted.canonical_bytes,
        value_indexes: value_indexes.clone(),
        inactive_slots,
        next_slot: state.next_slot,
        attempts: state.attempts.clone(),
        call_clocks: state.call_clocks.clone(),
        pending_calls: state.pending_calls.clone(),
        receipt_refs: state.receipt_refs.clone(),
        checkpoint_ref: state.checkpoint_ref.clone(),
        completed_calls: state.completed_calls,
        failed_calls: state.failed_calls,
        rejected_calls: state.rejected_calls,
        unknown_calls: state.unknown_calls,
        retained_screenshot_count: state.retained_screenshot_count,
        retained_screenshot_bytes: state.retained_screenshot_bytes,
        interaction_observation_invalidated: state.interaction_observation_invalidated,
    };
    let mut parents = vec![state.admitted.source_ref.artifact.clone()];
    if let Some(previous) = state.checkpoint_ref.as_ref() {
        parents.push(previous.artifact.clone());
    }
    parents.extend(value_indexes.iter().map(|index| index.artifact.clone()));
    parents.extend(
        state
            .receipt_refs
            .iter()
            .map(|receipt| receipt.artifact.clone()),
    );
    let (checkpoint, _) = put_evidence(store, "OrchCheckpointV1", &durable, parents).await?;
    state.checkpoint_ref = Some(checkpoint.clone());
    Ok(checkpoint)
}

pub(crate) async fn persist_terminal(
    store: &HubStoreHandle,
    terminal: &ScriptTerminalV1,
    source: Option<&InstructEvidenceRef>,
) -> Result<InstructEvidenceRef, haider_protocol::error::HaiderError> {
    let mut parents = Vec::new();
    if let Some(source) = source {
        parents.push(source.artifact.clone());
    }
    if let Some(checkpoint) = terminal.final_checkpoint.as_ref() {
        parents.push(checkpoint.artifact.clone());
    }
    parents.extend(
        terminal
            .receipt_refs
            .iter()
            .map(|receipt| receipt.artifact.clone()),
    );
    put_evidence(store, "OrchTerminalV1", terminal, parents)
        .await
        .map(|(evidence_ref, _)| evidence_ref)
}

pub(crate) async fn recover_checkpoint(
    store: &HubStoreHandle,
    script_id: &str,
    wrappers: Option<&WrapperSnapshot>,
) -> Result<Option<RuntimeStateV1>, haider_protocol::error::HaiderError> {
    let mut cursor = 0;
    let mut latest = None;
    loop {
        let page = StoreHandle::read_reducer_page_with_boundary(
            store,
            store.session_id(),
            cursor,
            REF_SCAN_PAGE,
            REF_SCAN_BYTES,
            &["item"],
        )
        .await?
        .envelopes;
        if page.is_empty() {
            break;
        }
        for envelope in page {
            cursor = envelope.seq;
            let Ok(EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            })) = envelope.payload.decode_event()
            else {
                continue;
            };
            if kind != haider_protocol::orchestration::ORCHESTRATION_CHECKPOINT_EXTENSION
                || data.get("script_id").and_then(Value::as_str) != Some(script_id)
            {
                continue;
            }
            if let Some(value) = data.get("checkpoint_ref")
                && let Ok(checkpoint) = serde_json::from_value::<InstructEvidenceRef>(value.clone())
            {
                latest = Some(checkpoint);
            }
        }
    }
    let Some(checkpoint) = latest else {
        return Ok(None);
    };
    let bytes = read_evidence(store, &checkpoint, "OrchCheckpointV1", script_id)
        .await
        .map_err(|failure| {
            haider_protocol::error::HaiderError::new(
                haider_protocol::error::ErrorCode::StoreCorrupt,
                failure.message,
                false,
            )
        })?;
    let durable = serde_json::from_slice::<DurableCheckpointV1>(&bytes).map_err(|error| {
        haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::StoreCorrupt,
            format!("orchestration checkpoint is malformed: {error}"),
            false,
        )
    })?;
    if durable.version != ORCHESTRATION_VERSION || durable.script_id != script_id {
        return Err(haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::StoreCorrupt,
            "orchestration checkpoint identity changed",
            false,
        ));
    }
    let corrupt = |message: String| {
        haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::StoreCorrupt,
            message,
            false,
        )
    };
    let source_bytes = read_evidence(store, &durable.source_ref, "OrchScriptV1", script_id)
        .await
        .map_err(|failure| corrupt(failure.message))?;
    let manifest = serde_json::from_slice::<ScriptManifestV1>(&source_bytes)
        .map_err(|error| corrupt(format!("orchestration source is malformed: {error}")))?;
    verify_canonical(&manifest, &source_bytes, script_id)
        .map_err(|failure| corrupt(failure.message))?;
    let expected_source_parents = std::iter::once(manifest.shape.artifact.clone())
        .chain(manifest.inputs.iter().map(|input| input.artifact.clone()))
        .collect::<Vec<_>>();
    if manifest.version != ORCHESTRATION_VERSION
        || manifest.transport != ORCHESTRATION_TRANSPORT
        || manifest.codec != ORCHESTRATION_CODEC
        || manifest.semantics != ORCHESTRATION_SEMANTICS
        || wrappers.is_some_and(|wrappers| manifest.catalog_digest != wrappers.catalog_digest)
        || durable.source_ref.parents != expected_source_parents
    {
        return Err(corrupt(
            "orchestration source identity or catalog changed".into(),
        ));
    }
    let imported = import_shape(&manifest.shape, &manifest.catalog_digest, store, script_id)
        .await
        .map_err(|failure| corrupt(failure.message))?;
    validate_inline_dag(
        &imported.parameters,
        &imported.nodes,
        &imported.exits,
        &imported.read_groups,
        Some(&manifest.limits),
    )
    .map_err(|error| corrupt(format!("recovered orchestration graph is invalid: {error}")))?;
    if let Some(wrappers) = wrappers {
        validate_wrappers(&imported.nodes, wrappers, script_id)
            .map_err(|failure| corrupt(failure.message))?;
        validate_read_group_wrappers(&imported.nodes, &imported.read_groups, wrappers, script_id)
            .map_err(|failure| corrupt(failure.message))?;
    }
    if manifest.inputs.len() != imported.parameters.len() {
        return Err(corrupt(
            "orchestration source input directory length changed".into(),
        ));
    }
    let mut inputs = Vec::with_capacity(manifest.inputs.len());
    let mut canonical_bytes = imported
        .canonical_bytes
        .saturating_add(source_bytes.len() as u64);
    for (ordinal, (input_ref, parameter)) in
        manifest.inputs.iter().zip(&imported.parameters).enumerate()
    {
        let input_bytes = read_evidence(store, input_ref, "OrchResultV1", script_id)
            .await
            .map_err(|failure| corrupt(failure.message))?;
        let payload: Value = serde_json::from_slice(&input_bytes)
            .map_err(|error| corrupt(format!("orchestration input is malformed: {error}")))?;
        verify_canonical(&payload, &input_bytes, script_id)
            .map_err(|failure| corrupt(failure.message))?;
        let payload_type = payload
            .get("type")
            .cloned()
            .and_then(|value| serde_json::from_value::<OrchTypeV1>(value).ok());
        let value = payload.get("value").cloned();
        if payload.get("version").and_then(Value::as_u64) != Some(u64::from(ORCHESTRATION_VERSION))
            || payload.get("kind").and_then(Value::as_str) != Some("input")
            || payload.get("ordinal").and_then(Value::as_u64) != Some(ordinal as u64)
            || payload_type.as_ref() != Some(parameter)
            || value
                .as_ref()
                .is_none_or(|value| validate_value(value, parameter, 0).is_err())
        {
            return Err(corrupt(format!(
                "orchestration input {ordinal} identity or type changed"
            )));
        }
        canonical_bytes = canonical_bytes.saturating_add(input_bytes.len() as u64);
        inputs.push(StrictJson(value.unwrap_or(Value::Null)));
    }
    if let Some(wrappers) = wrappers {
        validate_preflight_arguments(&imported.nodes, &inputs, wrappers, script_id)
            .map_err(|failure| corrupt(failure.message))?;
    }
    if durable.canonical_bytes != canonical_bytes {
        return Err(corrupt(
            "orchestration canonical byte accounting changed".into(),
        ));
    }
    let node_count = imported.nodes.len();
    if durable.next_slot as usize > node_count
        || durable.value_indexes.len() > node_count.div_ceil(256)
        || durable.attempts.iter().any(|(slot, attempts)| {
            *slot as usize >= node_count
                || imported.nodes[*slot as usize].evidence_type != "OrchCallV1"
                || !(1..=3).contains(attempts)
        })
        || durable.call_clocks.iter().any(|(slot, clock)| {
            *slot as usize >= node_count
                || imported.nodes[*slot as usize].evidence_type != "OrchCallV1"
                || clock.deadline_ms < clock.started_at_ms
                || clock
                    .retry_not_before_ms
                    .is_some_and(|retry| retry < clock.started_at_ms)
        })
        || durable.pending_calls.iter().any(|(slot, pending)| {
            slot != &pending.slot
                || *slot as usize >= node_count
                || pending.deadline_ms < pending.started_at_ms
                || durable.attempts.get(slot) != Some(&pending.attempt)
                || durable
                    .call_clocks
                    .get(slot)
                    .is_none_or(|clock| clock.deadline_ms != pending.deadline_ms)
                || !pending.started
                || imported.nodes[*slot as usize].config.0["tool"].as_str()
                    != Some(pending.tool.as_str())
                || pending.item_id
                    != format!(
                        "orch-call-{}-{}-{}",
                        durable.script_id, pending.slot, pending.attempt
                    )
                || pending.call_id
                    != format!(
                        "orch:{}:{}:{}",
                        durable.script_id, pending.slot, pending.attempt
                    )
        })
    {
        return Err(haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::StoreCorrupt,
            "orchestration checkpoint frontier or clock is invalid",
            false,
        ));
    }
    let mut values = vec![None; node_count];
    let mut value_refs = vec![None; node_count];
    let mut previous_inactive = None;
    for slot in &durable.inactive_slots {
        if *slot as usize >= node_count
            || previous_inactive.is_some_and(|previous| previous >= *slot)
        {
            return Err(corrupt(
                "orchestration inactive slot directory is invalid".into(),
            ));
        }
        values[*slot as usize] = Some(RuntimeValueV1::Inactive);
        previous_inactive = Some(*slot);
    }
    let mut previous_ready = None;
    for index_ref in &durable.value_indexes {
        let index_bytes = read_evidence(store, index_ref, "OrchIndexV1", script_id)
            .await
            .map_err(|failure| corrupt(failure.message))?;
        let index = serde_json::from_slice::<OrchIndexV1>(&index_bytes)
            .map_err(|error| corrupt(format!("orchestration value index is malformed: {error}")))?;
        verify_canonical(&index, &index_bytes, script_id)
            .map_err(|failure| corrupt(failure.message))?;
        if index.version != ORCHESTRATION_VERSION
            || index.domain != OrchIndexDomainV1::Execution
            || index.script_id.as_deref() != Some(script_id)
            || index.entries.is_empty()
            || index.entries.len() > 256
            || index_ref.parents
                != index
                    .entries
                    .iter()
                    .map(|entry| entry.evidence_ref.artifact.clone())
                    .collect::<Vec<_>>()
        {
            return Err(corrupt("orchestration value index identity changed".into()));
        }
        for entry in index.entries {
            if entry.slot as usize >= node_count
                || previous_ready.is_some_and(|previous| previous >= entry.slot)
                || values[entry.slot as usize].is_some()
            {
                return Err(corrupt(
                    "orchestration value index slots are invalid".into(),
                ));
            }
            let payload_bytes =
                read_evidence(store, &entry.evidence_ref, "OrchResultV1", script_id)
                    .await
                    .map_err(|failure| corrupt(failure.message))?;
            let payload: Value = serde_json::from_slice(&payload_bytes).map_err(|error| {
                corrupt(format!(
                    "orchestration value evidence is malformed: {error}"
                ))
            })?;
            if payload.get("script_id").and_then(Value::as_str) != Some(script_id)
                || payload.get("slot").and_then(Value::as_u64) != Some(u64::from(entry.slot))
            {
                return Err(corrupt(
                    "orchestration value evidence identity changed".into(),
                ));
            }
            let value = payload
                .get("value")
                .or_else(|| payload.get("result"))
                .cloned()
                .ok_or_else(|| {
                    corrupt("orchestration value evidence has no value/result".into())
                })?;
            values[entry.slot as usize] = Some(RuntimeValueV1::Ready {
                value: StrictJson(value),
            });
            value_refs[entry.slot as usize] = Some(entry.evidence_ref);
            previous_ready = Some(entry.slot);
        }
    }
    for receipt in &durable.receipt_refs {
        receipt
            .validate()
            .map_err(|error| corrupt(format!("orchestration receipt ref is invalid: {error}")))?;
        if receipt.evidence_type != "OrchResultV1" {
            return Err(corrupt(
                "orchestration receipt ref has the wrong evidence type".into(),
            ));
        }
    }
    if let Some(previous) = &durable.checkpoint_ref {
        previous.validate().map_err(|error| {
            corrupt(format!(
                "previous orchestration checkpoint ref is invalid: {error}"
            ))
        })?;
        if previous.evidence_type != "OrchCheckpointV1" {
            return Err(corrupt(
                "previous orchestration checkpoint ref has the wrong evidence type".into(),
            ));
        }
    }
    for (slot, pending) in &durable.pending_calls {
        pending.activation_ref.validate().map_err(|error| {
            corrupt(format!(
                "orchestration activation ref for slot {slot} is invalid: {error}"
            ))
        })?;
        let expected_activation_prefix = [
            durable.source_ref.artifact.clone(),
            imported.definition_refs[*slot as usize].artifact.clone(),
        ];
        if pending.activation_ref.evidence_type != "OrchActivationV1"
            || !pending
                .activation_ref
                .parents
                .starts_with(&expected_activation_prefix)
            || wrappers.is_some_and(|wrappers| {
                wrappers.wrappers.get(&pending.tool).is_none_or(|wrapper| {
                    validate_wrapper_argument(wrapper, &pending.args.0).is_err()
                })
            })
        {
            return Err(corrupt(format!(
                "orchestration pending call {slot} identity changed"
            )));
        }
    }
    let expected_checkpoint_parents = std::iter::once(durable.source_ref.artifact.clone())
        .chain(
            durable
                .checkpoint_ref
                .iter()
                .map(|previous| previous.artifact.clone()),
        )
        .chain(
            durable
                .value_indexes
                .iter()
                .map(|index| index.artifact.clone()),
        )
        .chain(
            durable
                .receipt_refs
                .iter()
                .map(|receipt| receipt.artifact.clone()),
        )
        .collect::<Vec<_>>();
    if checkpoint.parents != expected_checkpoint_parents {
        return Err(corrupt(
            "orchestration checkpoint ordered parents changed".into(),
        ));
    }
    let admitted = AdmittedScriptV1 {
        version: ORCHESTRATION_VERSION,
        script_id: durable.script_id,
        request_digest: durable.request_digest,
        shape_ref: manifest.shape,
        source_ref: durable.source_ref,
        definition_refs: imported.definition_refs,
        parameters: imported.parameters,
        nodes: imported.nodes,
        exits: imported.exits,
        read_groups: imported.read_groups,
        inputs,
        limits: manifest.limits,
        admitted_at_ms: durable.admitted_at_ms,
        canonical_bytes,
    };
    Ok(Some(RuntimeStateV1 {
        admitted,
        values,
        value_refs,
        next_slot: durable.next_slot,
        attempts: durable.attempts,
        call_clocks: durable.call_clocks,
        pending_calls: durable.pending_calls,
        receipt_refs: durable.receipt_refs,
        checkpoint_ref: durable.checkpoint_ref,
        completed_calls: durable.completed_calls,
        failed_calls: durable.failed_calls,
        rejected_calls: durable.rejected_calls,
        unknown_calls: durable.unknown_calls,
        retained_screenshot_count: durable.retained_screenshot_count,
        retained_screenshot_bytes: durable.retained_screenshot_bytes,
        interaction_observation_invalidated: durable.interaction_observation_invalidated,
    }))
}

pub(crate) async fn recover_terminal(
    store: &HubStoreHandle,
    script_id: &str,
) -> Result<Option<ScriptTerminalV1>, haider_protocol::error::HaiderError> {
    let mut cursor = 0;
    let mut terminal = None;
    loop {
        let page = StoreHandle::read_reducer_page_with_boundary(
            store,
            store.session_id(),
            cursor,
            REF_SCAN_PAGE,
            REF_SCAN_BYTES,
            &["item"],
        )
        .await?
        .envelopes;
        if page.is_empty() {
            break;
        }
        for envelope in page {
            cursor = envelope.seq;
            let Ok(EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            })) = envelope.payload.decode_event()
            else {
                continue;
            };
            if kind != haider_protocol::orchestration::ORCHESTRATION_TERMINAL_EXTENSION
                || data.get("script_id").and_then(Value::as_str) != Some(script_id)
            {
                continue;
            }
            let decoded = serde_json::from_value::<ScriptTerminalV1>(data).map_err(|error| {
                haider_protocol::error::HaiderError::new(
                    haider_protocol::error::ErrorCode::StoreCorrupt,
                    format!("orchestration terminal is malformed: {error}"),
                    false,
                )
            })?;
            terminal = Some(decoded);
        }
    }
    Ok(terminal)
}

fn effective_limits(requested: Option<OrchestrationLimitsV1>) -> OrchestrationLimitsV1 {
    let defaults = OrchestrationLimitsV1::default();
    let Some(requested) = requested else {
        return defaults;
    };
    OrchestrationLimitsV1 {
        definition_nodes: requested.definition_nodes.or(defaults.definition_nodes),
        parent_entries: requested.parent_entries.or(defaults.parent_entries),
        child_attempts: requested.child_attempts.or(defaults.child_attempts),
        script_wall_ms: requested.script_wall_ms.or(defaults.script_wall_ms),
        child_wall_ms: requested.child_wall_ms.or(defaults.child_wall_ms),
        no_progress_ms: requested.no_progress_ms.or(defaults.no_progress_ms),
        returned_bytes: requested.returned_bytes.or(defaults.returned_bytes),
        decision_points: requested.decision_points.or(defaults.decision_points),
        control_actions: requested.control_actions.or(defaults.control_actions),
        screenshots: requested.screenshots.or(defaults.screenshots),
    }
}

fn validate_wrappers(
    nodes: &[InlineNodeV1],
    wrappers: &WrapperSnapshot,
    request_digest: &str,
) -> Result<(), AdmissionFailure> {
    for node in nodes.iter().filter(|node| {
        matches!(
            node.evidence_type.as_str(),
            "OrchArgumentV1" | "OrchAskPauseV1" | "OrchCallV1"
        )
    }) {
        let tool = node
            .config
            .0
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let digest = node
            .config
            .0
            .get("wrapper_digest")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(wrapper) = wrappers.wrappers.get(tool) else {
            return Err(AdmissionFailure {
                request_digest: request_digest.into(),
                code: "wrapper_unavailable".into(),
                message: format!("slot {} names unavailable tool `{tool}`", node.slot),
            });
        };
        if wrapper.digest != digest {
            return Err(AdmissionFailure {
                request_digest: request_digest.into(),
                code: "wrapper_digest".into(),
                message: format!("slot {} has a stale wrapper digest", node.slot),
            });
        }
    }
    for node in nodes
        .iter()
        .filter(|node| node.evidence_type == "OrchCallV1")
    {
        let retry = node
            .ports
            .iter()
            .find(|port| port.role == OrchPortRoleV1::Config && port.port == "retry")
            .and_then(|port| nodes.get(port.source_slot as usize));
        let max_attempts = retry
            .and_then(|retry| retry.config.0.get("max_attempts"))
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let tool = node.config.0["tool"].as_str().unwrap_or_default();
        if max_attempts > 1
            && wrappers
                .wrappers
                .get(tool)
                .is_none_or(|wrapper| !wrapper.repeat_safe)
        {
            return Err(AdmissionFailure {
                request_digest: request_digest.into(),
                code: "retry_not_safe".into(),
                message: format!("slot {} retries non-repeat-safe tool `{tool}`", node.slot),
            });
        }
    }
    Ok(())
}

fn validate_preflight_arguments(
    nodes: &[InlineNodeV1],
    inputs: &[StrictJson],
    wrappers: &WrapperSnapshot,
    request_digest: &str,
) -> Result<(), AdmissionFailure> {
    let values = preflight_values(nodes, inputs);
    for node in nodes {
        if node.evidence_type != "OrchArgumentV1" {
            continue;
        }
        if let Some(argument) = values[node.slot as usize]
            .as_ref()
            .and_then(RuntimeValueV1::value)
        {
            let tool = node.config.0["tool"].as_str().unwrap_or_default();
            let Some(wrapper) = wrappers.wrappers.get(tool) else {
                return Err(AdmissionFailure {
                    request_digest: request_digest.into(),
                    code: "wrapper_unavailable".into(),
                    message: format!("slot {} names unavailable tool `{tool}`", node.slot),
                });
            };
            validate_wrapper_argument(wrapper, argument).map_err(|message| AdmissionFailure {
                request_digest: request_digest.into(),
                code: "wrapper_argument".into(),
                message: format!("slot {} argument for `{tool}`: {message}", node.slot),
            })?;
        }
    }
    Ok(())
}

fn preflight_values(nodes: &[InlineNodeV1], inputs: &[StrictJson]) -> Vec<Option<RuntimeValueV1>> {
    let mut values = vec![None; nodes.len()];
    for node in nodes {
        if matches!(
            node.evidence_type.as_str(),
            "OrchCallV1" | "OrchAwaitV1" | "OrchAskPauseV1"
        ) || node.ports.iter().any(|port| {
            matches!(port.role, OrchPortRoleV1::Data | OrchPortRoleV1::Guard)
                && values
                    .get(port.source_slot as usize)
                    .and_then(Option::as_ref)
                    .is_none()
        }) {
            continue;
        }
        // Pure evaluation failures remain runtime failures unless a later
        // admission rule needs the concrete value to prove safety.
        if let Ok(value) = evaluate_pure(node, nodes, &values, inputs) {
            values[node.slot as usize] = Some(value);
        }
    }
    values
}

#[derive(Debug)]
struct InteractionCall<'a> {
    slot: u32,
    tool: &'a str,
    action: String,
    region: &'a [Value],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionActionV1 {
    Screenshot,
    Observation,
    Control,
}

pub(crate) fn interaction_action(tool: &str, arguments: &Value) -> Option<InteractionActionV1> {
    let action = arguments.get("action")?.as_str()?;
    classify_interaction_action(tool, action)
}

fn classify_interaction_action(tool: &str, action: &str) -> Option<InteractionActionV1> {
    match (tool, action) {
        ("computer", "screenshot" | "inspect") | ("mobile", "screenshot") => {
            Some(InteractionActionV1::Screenshot)
        }
        ("computer", "cursor_position")
        | ("mobile", "a11y_tree" | "inspect" | "list_apps" | "sms_read") => {
            Some(InteractionActionV1::Observation)
        }
        (
            "computer",
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click"
            | "left_mouse_down" | "left_mouse_up" | "mouse_move" | "left_click_drag" | "type"
            | "key" | "scroll" | "wait",
        )
        | ("mobile", "tap" | "long_press" | "swipe" | "type" | "key" | "open_app") => {
            Some(InteractionActionV1::Control)
        }
        _ => None,
    }
}

fn static_string_field(
    nodes: &[InlineNodeV1],
    values: &[Option<RuntimeValueV1>],
    slot: usize,
    field: &str,
) -> Option<String> {
    if let Some(value) = values
        .get(slot)
        .and_then(Option::as_ref)
        .and_then(RuntimeValueV1::value)
        .and_then(|value| value.get(field))
        .and_then(Value::as_str)
    {
        return Some(value.to_owned());
    }
    let node = nodes.get(slot)?;
    if node.evidence_type == "OrchArgumentV1" {
        return node
            .ports
            .iter()
            .find(|port| port.role == OrchPortRoleV1::Data && port.port == "args")
            .and_then(|port| static_string_field(nodes, values, port.source_slot as usize, field));
    }
    if node.evidence_type != "OrchValueV1" || node.config.0["operator"] != "record" {
        return None;
    }
    let index = node.config.0["operand_config"]["fields"]
        .as_array()?
        .iter()
        .position(|candidate| candidate.as_str() == Some(field))?;
    let source = node
        .ports
        .iter()
        .filter(|port| port.role == OrchPortRoleV1::Data)
        .nth(index)?
        .source_slot as usize;
    values
        .get(source)
        .and_then(Option::as_ref)
        .and_then(RuntimeValueV1::value)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            let source = nodes.get(source)?;
            (source.evidence_type == "OrchValueV1" && source.config.0["operator"] == "literal")
                .then(|| {
                    source.config.0["operand_config"]["value"]
                        .as_str()
                        .map(str::to_owned)
                })
                .flatten()
        })
}

fn validate_interaction_sequences(
    nodes: &[InlineNodeV1],
    inputs: &[StrictJson],
    limits: &OrchestrationLimitsV1,
    request_digest: &str,
) -> Result<(), AdmissionFailure> {
    let values = preflight_values(nodes, inputs);
    let mut calls = Vec::new();
    for node in nodes
        .iter()
        .filter(|node| node.evidence_type == "OrchCallV1")
    {
        let tool = node.config.0["tool"].as_str().unwrap_or_default();
        if !matches!(tool, "computer" | "mobile") {
            continue;
        }
        let argument_slot = node
            .ports
            .iter()
            .find(|port| port.role == OrchPortRoleV1::Data && port.port == "args")
            .map(|port| port.source_slot as usize);
        let argument = argument_slot
            .and_then(|slot| values.get(slot))
            .and_then(Option::as_ref)
            .and_then(RuntimeValueV1::value);
        let action = argument
            .and_then(|value| value.get("action"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                argument_slot.and_then(|slot| static_string_field(nodes, &values, slot, "action"))
            })
            .ok_or_else(|| AdmissionFailure {
                request_digest: request_digest.into(),
                code: "interaction_argument".into(),
                message: format!(
                    "slot {} `{tool}` action must be statically provable at admission",
                    node.slot
                ),
            })?;
        let region = node
            .config
            .0
            .get("region")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        calls.push(InteractionCall {
            slot: node.slot,
            tool,
            action,
            region,
        });
    }
    if calls.is_empty() {
        return Ok(());
    }

    let candidate_paths = nodes
        .iter()
        .filter_map(|node| node.config.0.get("region").and_then(Value::as_array))
        .map(Vec::as_slice)
        .collect::<Vec<_>>();
    let mut paths = Vec::new();
    for path in candidate_paths.into_iter().chain(std::iter::once(&[][..])) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let paths = paths
        .iter()
        .copied()
        .filter(|candidate| {
            !paths
                .iter()
                .any(|other| other.len() > candidate.len() && other.starts_with(candidate))
        })
        .collect::<Vec<_>>();

    for path in paths {
        let mut path_screenshots = 0_u32;
        let mut path_controls = 0_u32;
        for tool in ["computer", "mobile"] {
            let path_calls = calls
                .iter()
                .filter(|call| call.tool == tool && path.starts_with(call.region))
                .collect::<Vec<_>>();
            if path_calls.is_empty() {
                continue;
            }
            let mut screenshot_ready = false;
            let mut screenshots = 0_u32;
            let mut controls = 0_u32;
            for call in path_calls {
                match classify_interaction_action(tool, &call.action) {
                    Some(InteractionActionV1::Screenshot) => {
                        screenshots = screenshots.saturating_add(1);
                        screenshot_ready = true;
                    }
                    Some(InteractionActionV1::Observation) => {}
                    Some(InteractionActionV1::Control) => {
                        controls = controls.saturating_add(1);
                        if !screenshot_ready {
                            return Err(AdmissionFailure {
                                request_digest: request_digest.into(),
                                code: "interaction_observation".into(),
                                message: format!(
                                    "slot {} `{tool}` control action requires a fresh preceding screenshot",
                                    call.slot
                                ),
                            });
                        }
                        screenshot_ready = false;
                    }
                    None => {
                        return Err(AdmissionFailure {
                            request_digest: request_digest.into(),
                            code: "interaction_argument".into(),
                            message: format!(
                                "slot {} `{tool}` has an unknown statically resolved action `{}`",
                                call.slot, call.action
                            ),
                        });
                    }
                }
            }
            if controls > 0 && !screenshot_ready {
                return Err(AdmissionFailure {
                    request_digest: request_digest.into(),
                    code: "interaction_final_observation".into(),
                    message: format!(
                        "`{tool}` control sequence must end with a fresh screenshot observation"
                    ),
                });
            }
            path_controls = path_controls.saturating_add(controls);
            path_screenshots = path_screenshots.saturating_add(screenshots);
        }
        let decision_limit = limits.decision_points.unwrap_or(4);
        let control_limit = limits.control_actions.unwrap_or(4);
        let screenshot_limit = limits.screenshots.unwrap_or(5);
        if path_controls > decision_limit
            || path_controls > control_limit
            || path_screenshots > screenshot_limit
        {
            return Err(AdmissionFailure {
                request_digest: request_digest.into(),
                code: "interaction_limit".into(),
                message: format!(
                    "interaction path uses decisions={path_controls}, controls={path_controls}, screenshots={path_screenshots}; limits are {decision_limit}/{control_limit}/{screenshot_limit}"
                ),
            });
        }
    }
    Ok(())
}

pub(crate) fn validate_wrapper_argument(wrapper: &WrapperV1, value: &Value) -> Result<(), String> {
    validate_schema_value(&wrapper.input_schema, value, "$", &wrapper.input_schema)
}

fn validate_schema_value(
    schema: &Value,
    value: &Value,
    path: &str,
    root: &Value,
) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let Some(pointer) = reference.strip_prefix('#') else {
            return Err(format!(
                "{path}: external schema references are unsupported"
            ));
        };
        let target = root
            .pointer(pointer)
            .ok_or_else(|| format!("{path}: unresolved schema reference `{reference}`"))?;
        validate_schema_value(target, value, path, root)?;
    }
    if let Some(options) = schema.get("allOf").and_then(Value::as_array) {
        for option in options {
            validate_schema_value(option, value, path, root)?;
        }
    }
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array)
        && !options
            .iter()
            .any(|option| validate_schema_value(option, value, path, root).is_ok())
    {
        return Err(format!("{path}: value does not match any allowed schema"));
    }
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array)
        && options
            .iter()
            .filter(|option| validate_schema_value(option, value, path, root).is_ok())
            .count()
            != 1
    {
        return Err(format!("{path}: value does not match exactly one schema"));
    }
    if let Some(condition) = schema.get("if") {
        let branch = if validate_schema_value(condition, value, path, root).is_ok() {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if let Some(branch) = branch {
            validate_schema_value(branch, value, path, root)?;
        }
    }
    if let Some(expected) = schema.get("const")
        && value != expected
    {
        return Err(format!(
            "{path}: value does not match the required constant"
        ));
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array)
        && !variants.contains(value)
    {
        return Err(format!("{path}: value is outside the allowed enum"));
    }
    if let Some(expected) = schema.get("type") {
        let matches = |name: &str| match name {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        };
        let valid = expected.as_str().is_some_and(matches)
            || expected
                .as_array()
                .is_some_and(|types| types.iter().filter_map(Value::as_str).any(matches));
        if !valid {
            return Err(format!("{path}: value has the wrong JSON type"));
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(field) {
                    return Err(format!("{path}: required field `{field}` is missing"));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (field, child) in object {
            if let Some(field_schema) = properties.and_then(|properties| properties.get(field)) {
                validate_schema_value(field_schema, child, &format!("{path}.{field}"), root)?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(format!("{path}: unexpected field `{field}`"));
            }
        }
    }
    if let Some(array) = value.as_array() {
        if let Some(limit) = schema.get("minItems").and_then(Value::as_u64)
            && array.len() < limit as usize
        {
            return Err(format!("{path}: array is shorter than {limit}"));
        }
        if let Some(limit) = schema.get("maxItems").and_then(Value::as_u64)
            && array.len() > limit as usize
        {
            return Err(format!("{path}: array is longer than {limit}"));
        }
        if let Some(items) = schema.get("items") {
            for (index, child) in array.iter().enumerate() {
                validate_schema_value(items, child, &format!("{path}[{index}]"), root)?;
            }
        }
    }
    if let Some(text) = value.as_str() {
        if let Some(limit) = schema.get("minLength").and_then(Value::as_u64)
            && text.chars().count() < limit as usize
        {
            return Err(format!("{path}: string is shorter than {limit}"));
        }
        if let Some(limit) = schema.get("maxLength").and_then(Value::as_u64)
            && text.chars().count() > limit as usize
        {
            return Err(format!("{path}: string is longer than {limit}"));
        }
    }
    if let Some(integer) = value.as_i64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_i64)
            && integer < minimum
        {
            return Err(format!("{path}: integer is below {minimum}"));
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_i64)
            && integer > maximum
        {
            return Err(format!("{path}: integer is above {maximum}"));
        }
    }
    Ok(())
}

fn validate_read_group_wrappers(
    nodes: &[InlineNodeV1],
    groups: &[ReadGroupV1],
    wrappers: &WrapperSnapshot,
    request_digest: &str,
) -> Result<(), AdmissionFailure> {
    for group in groups {
        let mut region = None;
        for slot in &group.members {
            let node = &nodes[*slot as usize];
            let tool = node.config.0["tool"].as_str().unwrap_or_default();
            if wrappers
                .wrappers
                .get(tool)
                .is_none_or(|wrapper| !wrapper.parallel_safe)
            {
                return Err(AdmissionFailure {
                    request_digest: request_digest.into(),
                    code: "read_group_tool".into(),
                    message: format!(
                        "read group {} contains non-parallel-safe tool `{tool}`",
                        group.id
                    ),
                });
            }
            let current = node
                .config
                .0
                .get("region")
                .cloned()
                .unwrap_or(Value::Array(vec![]));
            if region.as_ref().is_some_and(|expected| expected != &current) {
                return Err(AdmissionFailure {
                    request_digest: request_digest.into(),
                    code: "read_group_region".into(),
                    message: format!("read group {} crosses structured regions", group.id),
                });
            }
            region = Some(current);
            for other in &group.members {
                if slot != other && depends_on(*slot, *other, nodes) {
                    return Err(AdmissionFailure {
                        request_digest: request_digest.into(),
                        code: "read_group_dependency".into(),
                        message: format!("read group {} contains dependent calls", group.id),
                    });
                }
            }
        }
    }
    Ok(())
}

fn depends_on(slot: u32, ancestor: u32, nodes: &[InlineNodeV1]) -> bool {
    let mut stack = vec![slot];
    let mut visited = std::collections::HashSet::new();
    while let Some(slot) = stack.pop() {
        if !visited.insert(slot) {
            continue;
        }
        let Some(node) = nodes.get(slot as usize) else {
            continue;
        };
        for port in &node.ports {
            if port.source_slot == ancestor {
                return true;
            }
            stack.push(port.source_slot);
        }
    }
    false
}

struct ImportedShape {
    parameters: Vec<OrchTypeV1>,
    nodes: Vec<InlineNodeV1>,
    exits: Vec<u32>,
    read_groups: Vec<ReadGroupV1>,
    definition_refs: Vec<InstructEvidenceRef>,
    canonical_bytes: u64,
}

async fn import_shape(
    root: &InstructEvidenceRef,
    expected_catalog_digest: &str,
    store: &HubStoreHandle,
    request_digest: &str,
) -> Result<ImportedShape, AdmissionFailure> {
    let root_bytes = read_evidence(store, root, "OrchShapeV1", request_digest).await?;
    let shape: OrchShapeV1 =
        serde_json::from_slice(&root_bytes).map_err(|error| AdmissionFailure {
            request_digest: request_digest.into(),
            code: "shape_decode".into(),
            message: error.to_string(),
        })?;
    verify_canonical(&shape, &root_bytes, request_digest)?;
    if shape.version != ORCHESTRATION_VERSION
        || shape.transport != ORCHESTRATION_TRANSPORT
        || shape.codec != ORCHESTRATION_CODEC
        || shape.semantics != ORCHESTRATION_SEMANTICS
        || shape.catalog_digest != expected_catalog_digest
        || root.parents
            != shape
                .indexes
                .iter()
                .map(|index| index.artifact.clone())
                .collect::<Vec<_>>()
    {
        return Err(AdmissionFailure {
            request_digest: request_digest.into(),
            code: "shape_metadata".into(),
            message: "shape metadata or ordered index parents do not match".into(),
        });
    }
    let definition_count = shape.definition_count as usize;
    let expected_indexes = definition_count.div_ceil(256);
    if definition_count == 0
        || definition_count > ORCHESTRATION_NODE_MAX
        || shape.indexes.len() != expected_indexes
    {
        return Err(AdmissionFailure {
            request_digest: request_digest.into(),
            code: "shape_bounds".into(),
            message: "shape definition/index directory is outside frozen bounds".into(),
        });
    }
    let mut definition_refs = Vec::new();
    let mut total = root_bytes.len() as u64;
    for (page_ordinal, index_ref) in shape.indexes.iter().enumerate() {
        let bytes = read_evidence(store, index_ref, "OrchIndexV1", request_digest).await?;
        let index: OrchIndexV1 =
            serde_json::from_slice(&bytes).map_err(|error| AdmissionFailure {
                request_digest: request_digest.into(),
                code: "index_decode".into(),
                message: error.to_string(),
            })?;
        verify_canonical(&index, &bytes, request_digest)?;
        let first_slot = page_ordinal * 256;
        let expected_entries = (definition_count - first_slot).min(256);
        if index.version != ORCHESTRATION_VERSION
            || index.domain != OrchIndexDomainV1::Definition
            || index.script_id.is_some()
            || index.entries.len() != expected_entries
            || index
                .entries
                .iter()
                .enumerate()
                .any(|(offset, entry)| entry.slot as usize != first_slot.saturating_add(offset))
            || index_ref.parents
                != index
                    .entries
                    .iter()
                    .map(|entry| entry.evidence_ref.artifact.clone())
                    .collect::<Vec<_>>()
        {
            return Err(AdmissionFailure {
                request_digest: request_digest.into(),
                code: "index_metadata".into(),
                message: "definition index is malformed".into(),
            });
        }
        total = total.saturating_add(bytes.len() as u64);
        definition_refs.extend(index.entries.into_iter().map(|entry| entry.evidence_ref));
    }
    if definition_refs.len() != definition_count {
        return Err(AdmissionFailure {
            request_digest: request_digest.into(),
            code: "definition_count".into(),
            message: "shape definition count does not match its directory".into(),
        });
    }
    let by_artifact = definition_refs
        .iter()
        .map(|entry| (entry.artifact.clone(), entry.clone()))
        .collect::<HashMap<_, _>>();
    let mut nodes = Vec::with_capacity(definition_refs.len());
    for (expected_slot, definition_ref) in definition_refs.iter().enumerate() {
        let bytes = read_evidence(
            store,
            definition_ref,
            &definition_ref.evidence_type,
            request_digest,
        )
        .await?;
        let definition: MaterializedDefinitionV1 =
            serde_json::from_slice(&bytes).map_err(|error| AdmissionFailure {
                request_digest: request_digest.into(),
                code: "definition_decode".into(),
                message: error.to_string(),
            })?;
        verify_canonical(&definition, &bytes, request_digest)?;
        if definition.version != ORCHESTRATION_VERSION
            || definition.slot as usize != expected_slot
            || definition.evidence_type != definition_ref.evidence_type
            || definition_ref.parents
                != definition
                    .ports
                    .iter()
                    .map(|port| port.parent.clone())
                    .collect::<Vec<_>>()
        {
            return Err(AdmissionFailure {
                request_digest: request_digest.into(),
                code: "definition_metadata".into(),
                message: format!("definition slot {expected_slot} metadata does not match"),
            });
        }
        let ports = definition
            .ports
            .into_iter()
            .map(|port| {
                let parent = by_artifact
                    .get(&port.parent)
                    .ok_or_else(|| AdmissionFailure {
                        request_digest: request_digest.into(),
                        code: "foreign_parent".into(),
                        message: format!("definition slot {expected_slot} has a foreign parent"),
                    })?;
                if parent.ledger_digest != port.parent_ledger_digest {
                    return Err(AdmissionFailure {
                        request_digest: request_digest.into(),
                        code: "parent_ledger".into(),
                        message: format!("definition slot {expected_slot} parent ledger changed"),
                    });
                }
                Ok(InlinePortV1 {
                    role: port.role,
                    port: port.port,
                    source_slot: definition_refs
                        .iter()
                        .position(|candidate| candidate.artifact == parent.artifact)
                        .and_then(|slot| u32::try_from(slot).ok())
                        .unwrap_or(u32::MAX),
                    output: port.output,
                    selector: port.selector,
                })
            })
            .collect::<Result<Vec<_>, AdmissionFailure>>()?;
        total = total.saturating_add(bytes.len() as u64);
        nodes.push(InlineNodeV1 {
            slot: definition.slot,
            evidence_type: definition.evidence_type,
            config: definition.config,
            ports,
        });
    }
    Ok(ImportedShape {
        parameters: shape.parameters,
        nodes,
        exits: shape.exits,
        read_groups: shape.read_groups,
        definition_refs,
        canonical_bytes: total,
    })
}

async fn read_evidence(
    store: &HubStoreHandle,
    evidence_ref: &InstructEvidenceRef,
    expected_type: &str,
    request_digest: &str,
) -> Result<Vec<u8>, AdmissionFailure> {
    evidence_ref.validate().map_err(|error| AdmissionFailure {
        request_digest: request_digest.into(),
        code: "evidence_ledger".into(),
        message: error.to_string(),
    })?;
    if evidence_ref.evidence_type != expected_type {
        return Err(AdmissionFailure {
            request_digest: request_digest.into(),
            code: "evidence_type".into(),
            message: format!(
                "expected {expected_type}, got {}",
                evidence_ref.evidence_type
            ),
        });
    }
    if evidence_ref.byte_len == 0 || evidence_ref.byte_len > INSTRUCT_EVIDENCE_MAX_BYTES {
        return Err(AdmissionFailure {
            request_digest: request_digest.into(),
            code: "evidence_size".into(),
            message: "evidence byte length is outside 1..=1 MiB".into(),
        });
    }
    let bytes = store
        .get_artifact_bounded(evidence_ref.artifact.clone(), evidence_ref.byte_len)
        .await
        .map_err(|error| admission_store_failure(request_digest, error))?;
    if bytes.len() as u64 != evidence_ref.byte_len
        || format!("blake3:{}", blake3::hash(&bytes).to_hex()) != evidence_ref.artifact.as_str()
    {
        return Err(AdmissionFailure {
            request_digest: request_digest.into(),
            code: "evidence_bytes".into(),
            message: "evidence byte length or content address does not match".into(),
        });
    }
    Ok(bytes)
}

fn verify_canonical<T: Serialize>(
    value: &T,
    bytes: &[u8],
    request_digest: &str,
) -> Result<(), AdmissionFailure> {
    if canonical_json(value).is_ok_and(|canonical| canonical == bytes) {
        Ok(())
    } else {
        Err(AdmissionFailure {
            request_digest: request_digest.into(),
            code: "canonical_bytes".into(),
            message: "evidence bytes are not canonical JSON".into(),
        })
    }
}

async fn session_has_shape_access(
    store: &HubStoreHandle,
    root: &InstructEvidenceRef,
) -> Result<bool, haider_protocol::error::HaiderError> {
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read_reducer_page_with_boundary(
            store,
            store.session_id(),
            cursor,
            REF_SCAN_PAGE,
            REF_SCAN_BYTES,
            &["item"],
        )
        .await?
        .envelopes;
        if page.is_empty() {
            return Ok(false);
        }
        for envelope in &page {
            cursor = envelope.seq;
            let Ok(EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            })) = envelope.payload.decode_event()
            else {
                continue;
            };
            if kind == ORCHESTRATION_SCRIPT_EXTENSION
                && data
                    .get("shape_ref")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<InstructEvidenceRef>(value).ok())
                    .as_ref()
                    == Some(root)
            {
                return Ok(true);
            }
        }
    }
}

fn admission_store_failure(
    request_digest: &str,
    error: haider_protocol::error::HaiderError,
) -> AdmissionFailure {
    AdmissionFailure {
        request_digest: request_digest.into(),
        code: "storage".into(),
        message: error.message,
    }
}

pub(crate) fn active(node: &InlineNodeV1, values: &[Option<RuntimeValueV1>]) -> bool {
    node.ports
        .iter()
        .filter(|port| port.role == OrchPortRoleV1::Guard)
        .all(|guard| {
            values
                .get(guard.source_slot as usize)
                .and_then(Option::as_ref)
                .and_then(RuntimeValueV1::value)
                .and_then(Value::as_str)
                == guard.selector.as_deref()
        })
}

pub(crate) fn input_value<'a>(
    node: &InlineNodeV1,
    role: OrchPortRoleV1,
    port_name: &str,
    values: &'a [Option<RuntimeValueV1>],
) -> Option<&'a Value> {
    let port = node
        .ports
        .iter()
        .find(|port| port.role == role && port.port == port_name)?;
    values.get(port.source_slot as usize)?.as_ref()?.value()
}

pub(crate) fn evaluate_pure(
    node: &InlineNodeV1,
    nodes: &[InlineNodeV1],
    values: &[Option<RuntimeValueV1>],
    inputs: &[StrictJson],
) -> Result<RuntimeValueV1, String> {
    if !active(node, values) {
        return Ok(RuntimeValueV1::Inactive);
    }
    let data = || {
        node.ports
            .iter()
            .filter(|port| port.role == OrchPortRoleV1::Data)
            .map(|port| {
                values
                    .get(port.source_slot as usize)
                    .and_then(Option::as_ref)
                    .and_then(RuntimeValueV1::value)
                    .cloned()
                    .ok_or_else(|| {
                        format!("slot {} data port {} is not ready", node.slot, port.port)
                    })
            })
            .collect::<Result<Vec<_>, _>>()
    };
    let value = match node.evidence_type.as_str() {
        "OrchValueV1" => evaluate_value(node, data()?, inputs)?,
        "OrchArgumentV1" => data()?.into_iter().next().ok_or("argument has no value")?,
        "OrchRetryPolicyV1" => node.config.0.clone(),
        "OrchAskPauseV1" => Value::String("permit".into()),
        "OrchAwaitV1" => data()?.into_iter().next().ok_or("await has no operation")?,
        "OrchBranchV1" => {
            let subject = data()?.into_iter().next().ok_or("branch has no subject")?;
            let cases = node.config.0["cases"]
                .as_array()
                .ok_or("branch cases missing")?;
            let selected = if let Some(value) = subject.as_bool() {
                if value { "true" } else { "false" }
            } else {
                let source = node
                    .ports
                    .iter()
                    .find(|port| port.role == OrchPortRoleV1::Data && port.port == "subject")
                    .and_then(|port| nodes.get(port.source_slot as usize))
                    .ok_or("branch subject source is unavailable")?;
                if source.evidence_type == "OrchAwaitV1" {
                    if subject.get("status").and_then(Value::as_str) == Some("completed") {
                        "completed"
                    } else {
                        "recoverable_error"
                    }
                } else {
                    let discriminant = source.config.0["type"]["discriminant"]
                        .as_str()
                        .ok_or("branch union discriminant is unavailable")?;
                    subject
                        .get(discriminant)
                        .and_then(Value::as_str)
                        .ok_or("branch subject is not a tagged union")?
                }
            };
            if !cases.iter().any(|case| case.as_str() == Some(selected)) {
                return Err("branch subject is not covered by cases".into());
            }
            Value::String(selected.into())
        }
        "OrchJoinV1" => evaluate_join(node, nodes, values)?,
        "OrchExitV1" => data()?.into_iter().next().ok_or("exit has no value")?,
        other => return Err(format!("{other} is not a pure definition")),
    };
    if matches!(node.evidence_type.as_str(), "OrchValueV1" | "OrchJoinV1") {
        let descriptor: OrchTypeV1 = serde_json::from_value(node.config.0["type"].clone())
            .map_err(|error| format!("slot {} output type is invalid: {error}", node.slot))?;
        validate_value(&value, &descriptor, 0).map_err(|error| {
            format!(
                "slot {} output violates its declared type: {error}",
                node.slot
            )
        })?;
    }
    Ok(RuntimeValueV1::Ready {
        value: StrictJson(value),
    })
}

fn evaluate_join(
    node: &InlineNodeV1,
    nodes: &[InlineNodeV1],
    values: &[Option<RuntimeValueV1>],
) -> Result<Value, String> {
    match node.config.0["mode"].as_str().unwrap_or("all") {
        "all" => {
            let omit_inactive = node.config.0["omit_inactive"].as_bool().unwrap_or(false);
            let mut output = Vec::new();
            for port in node
                .ports
                .iter()
                .filter(|port| port.role == OrchPortRoleV1::Data)
            {
                match values
                    .get(port.source_slot as usize)
                    .and_then(Option::as_ref)
                {
                    Some(RuntimeValueV1::Ready { value }) if omit_inactive => {
                        let source = values
                            .get(port.source_slot as usize)
                            .and_then(Option::as_ref)
                            .and_then(RuntimeValueV1::value)
                            .ok_or("map select output is unavailable")?;
                        let record = source
                            .as_object()
                            .ok_or("map select output is not a tagged record")?;
                        let discriminant = nodes
                            .get(port.source_slot as usize)
                            .and_then(|source| source.config.0["type"]["discriminant"].as_str())
                            .ok_or("map select discriminant is unavailable")?;
                        match record.get(discriminant).and_then(Value::as_str) {
                            Some("present") => output.push(
                                record
                                    .get("value")
                                    .cloned()
                                    .ok_or("present map output has no value")?,
                            ),
                            Some("absent") => {}
                            _ => return Err("map select output has an invalid tag".into()),
                        }
                    }
                    Some(RuntimeValueV1::Ready { value }) => output.push(value.0.clone()),
                    Some(RuntimeValueV1::Inactive) if omit_inactive => {}
                    Some(RuntimeValueV1::Inactive) => {
                        return Err(
                            "all join encountered an inactive input without omit_inactive".into(),
                        );
                    }
                    None => return Err("all join input is not settled".into()),
                }
            }
            if output.is_empty()
                && node
                    .ports
                    .iter()
                    .all(|port| port.role != OrchPortRoleV1::Data)
            {
                Ok(Value::Null)
            } else {
                Ok(Value::Array(output))
            }
        }
        "select" => {
            let choice = input_value(node, OrchPortRoleV1::Data, "choice", values)
                .and_then(Value::as_str)
                .ok_or("select join has no choice")?;
            node.ports
                .iter()
                .find(|port| {
                    port.role == OrchPortRoleV1::Alternative
                        && port.selector.as_deref() == Some(choice)
                })
                .and_then(|port| values.get(port.source_slot as usize))
                .and_then(Option::as_ref)
                .and_then(RuntimeValueV1::value)
                .cloned()
                .ok_or_else(|| "selected join alternative is not ready".into())
        }
        _ => Err("unknown join mode".into()),
    }
}

fn evaluate_value(
    node: &InlineNodeV1,
    operands: Vec<Value>,
    inputs: &[StrictJson],
) -> Result<Value, String> {
    let operator = node.config.0["operator"]
        .as_str()
        .ok_or("value operator missing")?;
    let config = &node.config.0["operand_config"];
    match operator {
        "literal" => Ok(config["value"].clone()),
        "input" => config["parameter"]
            .as_u64()
            .and_then(|index| inputs.get(index as usize))
            .map(|input| input.0.clone())
            .ok_or_else(|| "input parameter is unavailable".into()),
        "get" => operands
            .first()
            .and_then(Value::as_object)
            .and_then(|record| config["field"].as_str().and_then(|field| record.get(field)))
            .cloned()
            .ok_or_else(|| "record field is unavailable".into()),
        "index" => {
            let index = operands
                .get(1)
                .and_then(Value::as_i64)
                .filter(|index| *index >= 0)
                .ok_or("list index is not a nonnegative i64")? as usize;
            operands
                .first()
                .and_then(Value::as_array)
                .and_then(|list| list.get(index))
                .cloned()
                .ok_or_else(|| "list index is out of bounds".into())
        }
        "record" => {
            let fields = config["fields"].as_array().ok_or("record fields missing")?;
            if fields.len() != operands.len() {
                return Err("record field/operand counts differ".into());
            }
            Ok(Value::Object(
                fields
                    .iter()
                    .zip(operands)
                    .map(|(field, value)| {
                        field
                            .as_str()
                            .map(|field| (field.into(), value))
                            .ok_or_else(|| "record field is not a string".to_owned())
                    })
                    .collect::<Result<_, _>>()?,
            ))
        }
        "list" => Ok(Value::Array(operands)),
        "builtin" => evaluate_builtin(
            config["name"].as_str().ok_or("builtin name missing")?,
            &operands,
            &node.config.0["type"],
        ),
        _ => Err("unknown value operator".into()),
    }
}

fn evaluate_builtin(name: &str, operands: &[Value], output_type: &Value) -> Result<Value, String> {
    let ints = || {
        Ok::<_, String>((
            operands
                .first()
                .and_then(Value::as_i64)
                .ok_or("left operand is not i64")?,
            operands
                .get(1)
                .and_then(Value::as_i64)
                .ok_or("right operand is not i64")?,
        ))
    };
    let integer = |value: Option<i64>| {
        value
            .map(Value::from)
            .ok_or_else(|| "checked arithmetic overflow".into())
    };
    match name {
        "add" => {
            let (a, b) = ints()?;
            integer(a.checked_add(b))
        }
        "sub" => {
            let (a, b) = ints()?;
            integer(a.checked_sub(b))
        }
        "mul" => {
            let (a, b) = ints()?;
            integer(a.checked_mul(b))
        }
        "div" => {
            let (a, b) = ints()?;
            integer(a.checked_div(b))
        }
        "rem" => {
            let (a, b) = ints()?;
            integer(a.checked_rem(b))
        }
        "eq" => Ok(Value::Bool(operands.first() == operands.get(1))),
        "lt" | "le" | "gt" | "ge" => {
            let (a, b) = ints()?;
            Ok(Value::Bool(match name {
                "lt" => a < b,
                "le" => a <= b,
                "gt" => a > b,
                _ => a >= b,
            }))
        }
        "and" | "or" => {
            let a = operands
                .first()
                .and_then(Value::as_bool)
                .ok_or("left operand is not bool")?;
            let b = operands
                .get(1)
                .and_then(Value::as_bool)
                .ok_or("right operand is not bool")?;
            Ok(Value::Bool(if name == "and" { a && b } else { a || b }))
        }
        "not" => Ok(Value::Bool(
            !operands
                .first()
                .and_then(Value::as_bool)
                .ok_or("operand is not bool")?,
        )),
        "len" => {
            let len = operands
                .first()
                .and_then(|value| {
                    value
                        .as_array()
                        .map(Vec::len)
                        .or_else(|| value.as_str().map(|text| text.chars().count()))
                })
                .ok_or("len operand is not list/string")?;
            Ok(Value::from(
                i64::try_from(len).map_err(|_| "length exceeds i64")?,
            ))
        }
        "sha256" => {
            let text = operands
                .first()
                .and_then(Value::as_str)
                .ok_or("sha256 operand is not string")?;
            Ok(Value::String(format!(
                "{:x}",
                Sha256::digest(text.as_bytes())
            )))
        }
        "concat" => Ok(Value::String(format!(
            "{}{}",
            operands
                .first()
                .and_then(Value::as_str)
                .ok_or("left concat operand is not string")?,
            operands
                .get(1)
                .and_then(Value::as_str)
                .ok_or("right concat operand is not string")?
        ))),
        "slice" => {
            let text = operands
                .first()
                .and_then(Value::as_str)
                .ok_or("slice operand is not string")?;
            let start = operands
                .get(1)
                .and_then(Value::as_i64)
                .filter(|value| *value >= 0)
                .ok_or("slice start invalid")? as usize;
            let end = operands
                .get(2)
                .and_then(Value::as_i64)
                .filter(|value| *value >= 0)
                .ok_or("slice end invalid")? as usize;
            if start > end {
                return Err("slice start exceeds end".into());
            }
            let chars = text.chars().collect::<Vec<_>>();
            if end > chars.len() {
                return Err("slice end is out of bounds".into());
            }
            Ok(Value::String(chars[start..end].iter().collect()))
        }
        "split" => {
            let text = operands
                .first()
                .and_then(Value::as_str)
                .ok_or("split operand is not string")?;
            let separator = operands
                .get(1)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("split separator is empty")?;
            Ok(Value::Array(
                text.split(separator)
                    .map(|part| Value::String(part.into()))
                    .collect(),
            ))
        }
        "repeat" => {
            let text = operands
                .first()
                .and_then(Value::as_str)
                .ok_or("repeat operand is not string")?;
            let count = operands
                .get(1)
                .and_then(Value::as_i64)
                .filter(|value| *value >= 0)
                .ok_or("repeat count invalid")? as usize;
            Ok(Value::String(text.repeat(count)))
        }
        "parse_json" => {
            let text = operands
                .first()
                .and_then(Value::as_str)
                .ok_or("parse_json operand is not string")?;
            let parsed: StrictJson = serde_json::from_str(text)
                .map_err(|error| format!("parse_json failed: {error}"))?;
            let descriptor: OrchTypeV1 = serde_json::from_value(output_type.clone())
                .map_err(|error| format!("parse_json output type failed: {error}"))?;
            validate_value(&parsed.0, &descriptor, 0)?;
            Ok(parsed.0)
        }
        _ => Err("unknown builtin".into()),
    }
}

pub(crate) fn validate_value(
    value: &Value,
    value_type: &OrchTypeV1,
    depth: usize,
) -> Result<(), String> {
    if depth > 32 {
        return Err("value nesting exceeds 32".into());
    }
    match value_type {
        OrchTypeV1::Null if value.is_null() => Ok(()),
        OrchTypeV1::Bool if value.is_boolean() => Ok(()),
        OrchTypeV1::I64 { min, max } => value
            .as_i64()
            .filter(|value| value >= min && value <= max)
            .map(|_| ())
            .ok_or_else(|| "value is outside declared i64 bounds".into()),
        OrchTypeV1::String { max_bytes } => value
            .as_str()
            .filter(|value| value.len() <= *max_bytes as usize)
            .map(|_| ())
            .ok_or_else(|| "value is outside declared string bounds".into()),
        OrchTypeV1::List { item, max_items } => {
            let list = value
                .as_array()
                .filter(|list| list.len() <= *max_items as usize)
                .ok_or("value is outside declared list bounds")?;
            for value in list {
                validate_value(value, item, depth + 1)?;
            }
            Ok(())
        }
        OrchTypeV1::Record { fields } => {
            let record = value
                .as_object()
                .filter(|record| record.len() == fields.len())
                .ok_or("value is not the declared closed record")?;
            for (field, field_type) in fields {
                validate_value(
                    record.get(field).ok_or("record field missing")?,
                    field_type,
                    depth + 1,
                )?;
            }
            Ok(())
        }
        OrchTypeV1::Union {
            discriminant,
            variants,
        } => {
            let record = value.as_object().ok_or("union value is not an object")?;
            let tag = record
                .get(discriminant)
                .and_then(Value::as_str)
                .ok_or("union discriminant missing")?;
            let variant = variants.get(tag).ok_or("union variant unknown")?;
            validate_value(value, variant, depth + 1)
        }
        OrchTypeV1::Opaque { .. } if value.is_object() => Ok(()),
        _ => Err("value does not match its declared type".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_join(omit_inactive: bool) -> InlineNodeV1 {
        InlineNodeV1 {
            slot: 2,
            evidence_type: "OrchJoinV1".into(),
            config: StrictJson(serde_json::json!({
                "mode":"all",
                "type":{"kind":"list","item":{"kind":"i64","min":0,"max":9},"max_items":2},
                "omit_inactive":omit_inactive,
                "region":[]
            })),
            ports: vec![
                InlinePortV1 {
                    role: OrchPortRoleV1::Data,
                    port: "first".into(),
                    source_slot: 0,
                    output: "value".into(),
                    selector: None,
                },
                InlinePortV1 {
                    role: OrchPortRoleV1::Data,
                    port: "second".into(),
                    source_slot: 1,
                    output: "value".into(),
                    selector: None,
                },
            ],
        }
    }

    #[test]
    fn map_join_extracts_present_values_and_omits_absent_values() {
        let source = |slot| InlineNodeV1 {
            slot,
            evidence_type: "OrchJoinV1".into(),
            config: StrictJson(serde_json::json!({
                "mode":"select",
                "type":{
                    "kind":"union","discriminant":"kind",
                    "variants":{
                        "absent":{"kind":"record","fields":{"kind":{"kind":"string","max_bytes":7}}},
                        "present":{"kind":"record","fields":{"kind":{"kind":"string","max_bytes":7},"value":{"kind":"i64","min":0,"max":9}}}
                    }
                },
                "omit_inactive":false,"region":[]
            })),
            ports: vec![],
        };
        let nodes = vec![source(0), source(1), all_join(true)];
        let values = vec![
            Some(RuntimeValueV1::Ready {
                value: StrictJson(serde_json::json!({"kind":"present","value":7})),
            }),
            Some(RuntimeValueV1::Ready {
                value: StrictJson(serde_json::json!({"kind":"absent"})),
            }),
        ];
        assert_eq!(
            evaluate_join(&nodes[2], &nodes, &values).expect("recognized map values evaluate"),
            serde_json::json!([7])
        );
    }

    #[test]
    fn ordinary_all_join_never_silently_drops_an_inactive_input() {
        let values = vec![
            Some(RuntimeValueV1::Ready {
                value: StrictJson(Value::from(7)),
            }),
            Some(RuntimeValueV1::Inactive),
        ];
        assert!(evaluate_join(&all_join(false), &[], &values).is_err());
    }

    fn interaction_nodes(tool: &str, actions: &[&str]) -> Vec<InlineNodeV1> {
        let mut nodes = Vec::new();
        for action in actions {
            let value_slot = nodes.len() as u32;
            nodes.push(InlineNodeV1 {
                slot: value_slot,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal",
                    "type":{"kind":"record","fields":{"action":{"kind":"string","max_bytes":32}}},
                    "operand_config":{"value":{"action":action}},
                    "region":[]
                })),
                ports: vec![],
            });
            let argument_slot = nodes.len() as u32;
            nodes.push(InlineNodeV1 {
                slot: argument_slot,
                evidence_type: "OrchArgumentV1".into(),
                config: StrictJson(serde_json::json!({
                    "tool":tool,
                    "wrapper_digest":"blake3:0000000000000000000000000000000000000000000000000000000000000000",
                    "region":[]
                })),
                ports: vec![InlinePortV1 {
                    role: OrchPortRoleV1::Data,
                    port: "args".into(),
                    source_slot: value_slot,
                    output: "value".into(),
                    selector: None,
                }],
            });
            let call_slot = nodes.len() as u32;
            nodes.push(InlineNodeV1 {
                slot: call_slot,
                evidence_type: "OrchCallV1".into(),
                config: StrictJson(serde_json::json!({
                    "tool":tool,
                    "wrapper_digest":"blake3:0000000000000000000000000000000000000000000000000000000000000000",
                    "region":[]
                })),
                ports: vec![InlinePortV1 {
                    role: OrchPortRoleV1::Data,
                    port: "args".into(),
                    source_slot: argument_slot,
                    output: "value".into(),
                    selector: None,
                }],
            });
        }
        nodes
    }

    #[test]
    fn interaction_sequence_requires_observe_action_observe() {
        let valid = interaction_nodes("mobile", &["screenshot", "tap", "screenshot"]);
        assert!(
            validate_interaction_sequences(
                &valid,
                &[],
                &OrchestrationLimitsV1::default(),
                "request",
            )
            .is_ok()
        );

        for actions in [
            &["tap", "screenshot"][..],
            &["screenshot", "tap"][..],
            &["screenshot", "tap", "tap", "screenshot"][..],
        ] {
            let invalid = interaction_nodes("mobile", actions);
            assert!(
                validate_interaction_sequences(
                    &invalid,
                    &[],
                    &OrchestrationLimitsV1::default(),
                    "request",
                )
                .is_err(),
                "accepted invalid interaction sequence {actions:?}"
            );
        }
    }

    #[test]
    fn interaction_action_is_provable_from_a_partially_dynamic_record() {
        let nodes = vec![
            InlineNodeV1 {
                slot: 0,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal",
                    "type":{"kind":"string","max_bytes":32},
                    "operand_config":{"value":"tap"},
                    "region":[]
                })),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 1,
                evidence_type: "OrchAwaitV1".into(),
                config: StrictJson(serde_json::json!({"on_error":"stop","region":[]})),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 2,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"record",
                    "type":{"kind":"record","fields":{
                        "action":{"kind":"string","max_bytes":32},
                        "x":{"kind":"i64"}
                    }},
                    "operand_config":{"fields":["action","x"]},
                    "region":[]
                })),
                ports: vec![
                    InlinePortV1 {
                        role: OrchPortRoleV1::Data,
                        port: "action".into(),
                        source_slot: 0,
                        output: "value".into(),
                        selector: None,
                    },
                    InlinePortV1 {
                        role: OrchPortRoleV1::Data,
                        port: "x".into(),
                        source_slot: 1,
                        output: "value".into(),
                        selector: None,
                    },
                ],
            },
            InlineNodeV1 {
                slot: 3,
                evidence_type: "OrchArgumentV1".into(),
                config: StrictJson(serde_json::json!({"tool":"mobile","region":[]})),
                ports: vec![InlinePortV1 {
                    role: OrchPortRoleV1::Data,
                    port: "args".into(),
                    source_slot: 2,
                    output: "value".into(),
                    selector: None,
                }],
            },
        ];
        let values = vec![None; nodes.len()];
        assert_eq!(
            static_string_field(&nodes, &values, 3, "action").as_deref(),
            Some("tap")
        );
    }

    #[test]
    fn interaction_sequence_enforces_default_decision_bound() {
        let nodes = interaction_nodes(
            "computer",
            &[
                "screenshot",
                "left_click",
                "screenshot",
                "left_click",
                "screenshot",
                "left_click",
                "screenshot",
                "left_click",
                "screenshot",
                "left_click",
                "screenshot",
            ],
        );
        let error = validate_interaction_sequences(
            &nodes,
            &[],
            &OrchestrationLimitsV1::default(),
            "request",
        )
        .expect_err("five decisions exceed the default bound");
        assert_eq!(error.code, "interaction_limit");
    }
}
