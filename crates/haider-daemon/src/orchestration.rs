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
    ORCHESTRATION_SCRIPT_EXTENSION, ORCHESTRATION_SEMANTICS, ORCHESTRATION_TRANSPORT,
    ORCHESTRATION_VERSION, OrchIndexDomainV1, OrchIndexEntryV1, OrchIndexV1, OrchPortRoleV1,
    OrchShapeV1, OrchTypeV1, OrchestrationError, OrchestrationLimitsV1, ReadGroupV1, ScriptGraphV1,
    ScriptRequestV1, ScriptTerminalV1, StrictJson, canonical_json, parse_script_request,
    request_digest, validate_inline_dag,
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
    pub(crate) started: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeStateV1 {
    pub(crate) admitted: AdmittedScriptV1,
    pub(crate) values: Vec<Option<RuntimeValueV1>>,
    pub(crate) next_slot: u32,
    pub(crate) attempts: BTreeMap<u32, u8>,
    pub(crate) pending_call: Option<PendingCallV1>,
    pub(crate) receipt_refs: Vec<InstructEvidenceRef>,
    pub(crate) checkpoint_ref: Option<InstructEvidenceRef>,
    pub(crate) completed_calls: u32,
    pub(crate) failed_calls: u32,
    pub(crate) rejected_calls: u32,
    pub(crate) unknown_calls: u32,
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

#[derive(Serialize)]
struct ScriptManifest<'a> {
    version: u32,
    transport: &'static str,
    codec: &'static str,
    semantics: &'static str,
    catalog_digest: &'a str,
    shape: &'a InstructEvidenceRef,
    inputs: Vec<InstructEvidenceRef>,
    limits: &'a Option<haider_protocol::orchestration::OrchestrationLimitsV1>,
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
                validate_inline_dag(
                    parameters,
                    nodes,
                    exits,
                    read_groups,
                    request.limits.as_ref(),
                )
                .map_err(|error| AdmissionFailure {
                    request_digest: digest.clone(),
                    code: error.code.into(),
                    message: error.to_string(),
                })?;
                validate_wrappers(nodes, wrappers, &digest)?;
                let materialized =
                    materialize_inline(parameters, nodes, exits, read_groups, &request, store)
                        .await
                        .map_err(|error| admission_store_failure(&digest, error))?;
                (
                    parameters.clone(),
                    nodes.clone(),
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
                let imported = import_shape(root, store, &digest).await?;
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
    let manifest = ScriptManifest {
        version: ORCHESTRATION_VERSION,
        transport: ORCHESTRATION_TRANSPORT,
        codec: ORCHESTRATION_CODEC,
        semantics: ORCHESTRATION_SEMANTICS,
        catalog_digest: &request.catalog_digest,
        shape: &shape_ref,
        inputs: input_refs.clone(),
        limits: &request.limits,
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
        next_slot: 0,
        attempts: BTreeMap::new(),
        pending_call: None,
        receipt_refs: Vec::new(),
        checkpoint_ref: None,
        completed_calls: 0,
        failed_calls: 0,
        rejected_calls: 0,
        unknown_calls: 0,
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
            limits: effective_limits(request.limits),
            admitted_at_ms,
            canonical_bytes: total_bytes,
        },
    })
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

pub(crate) async fn persist_checkpoint(
    store: &HubStoreHandle,
    state: &mut RuntimeStateV1,
) -> Result<InstructEvidenceRef, haider_protocol::error::HaiderError> {
    // The previous checkpoint is deliberately not a parent. Checkpoints are
    // complete replay states, not a chain a recovery reader must chase.
    let mut parents = vec![state.admitted.source_ref.artifact.clone()];
    parents.extend(
        state
            .receipt_refs
            .iter()
            .map(|receipt| receipt.artifact.clone()),
    );
    let (checkpoint, _) = put_evidence(store, "OrchCheckpointV1", state, parents).await?;
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
    let state = serde_json::from_slice::<RuntimeStateV1>(&bytes).map_err(|error| {
        haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::StoreCorrupt,
            format!("orchestration checkpoint is malformed: {error}"),
            false,
        )
    })?;
    if state.admitted.script_id != script_id {
        return Err(haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::StoreCorrupt,
            "orchestration checkpoint identity changed",
            false,
        ));
    }
    Ok(Some(state))
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
    let mut definition_refs = Vec::new();
    let mut total = root_bytes.len() as u64;
    for index_ref in &shape.indexes {
        let bytes = read_evidence(store, index_ref, "OrchIndexV1", request_digest).await?;
        let index: OrchIndexV1 =
            serde_json::from_slice(&bytes).map_err(|error| AdmissionFailure {
                request_digest: request_digest.into(),
                code: "index_decode".into(),
                message: error.to_string(),
            })?;
        verify_canonical(&index, &bytes, request_digest)?;
        if index.domain != OrchIndexDomainV1::Definition
            || index.script_id.is_some()
            || index.entries.len() > 256
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
    if definition_refs.len() != shape.definition_count as usize {
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
                    .and_then(|value| value.get("artifact"))
                    .and_then(Value::as_str)
                    == Some(root.artifact.as_str())
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
                subject
                    .as_object()
                    .and_then(|value| value.values().find_map(Value::as_str))
                    .ok_or("branch subject is not bool/tagged union")?
            };
            if !cases.iter().any(|case| case.as_str() == Some(selected)) {
                return Err("branch subject is not covered by cases".into());
            }
            Value::String(selected.into())
        }
        "OrchJoinV1" => evaluate_join(node, values)?,
        "OrchExitV1" => data()?.into_iter().next().ok_or("exit has no value")?,
        other => return Err(format!("{other} is not a pure definition")),
    };
    Ok(RuntimeValueV1::Ready {
        value: StrictJson(value),
    })
}

fn evaluate_join(node: &InlineNodeV1, values: &[Option<RuntimeValueV1>]) -> Result<Value, String> {
    match node.config.0["mode"].as_str().unwrap_or("all") {
        "all" => Ok(Value::Array(
            node.ports
                .iter()
                .filter(|port| port.role == OrchPortRoleV1::Data)
                .filter_map(|port| {
                    values
                        .get(port.source_slot as usize)
                        .and_then(Option::as_ref)
                        .and_then(RuntimeValueV1::value)
                        .cloned()
                })
                .collect(),
        )),
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
