//! Frozen Instruct-Pipe orchestration contracts.
//!
//! This module deliberately contains only bounded data and deterministic
//! validation/canonicalisation.  It is not a language runtime: an inline DAG
//! is an address-elided encoding of the same immutable rows accepted by the
//! ref form, and daemon execution must still dispatch every child through the
//! ordinary tool broker.

use crate::ids::ArtifactRef;
use crate::pipe::InstructEvidenceRef;
use crate::tool::ToolResultStatus;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;

pub const ORCHESTRATION_VERSION: u32 = 1;
pub const ORCHESTRATION_TRANSPORT: &str = "instruct-pipe-dag-v1";
pub const ORCHESTRATION_CODEC: &str = "canonical-json-v1";
pub const ORCHESTRATION_SEMANTICS: &str = "haider-orchestration-v1";

pub const ORCHESTRATION_RAW_DEFAULT_BYTES: usize = 32 * 1024;
pub const ORCHESTRATION_RAW_MAX_BYTES: usize = 128 * 1024;
pub const ORCHESTRATION_NODE_DEFAULT: usize = 2_048;
pub const ORCHESTRATION_NODE_MAX: usize = 8_192;
pub const ORCHESTRATION_EDGE_DEFAULT: usize = 8_192;
pub const ORCHESTRATION_EDGE_MAX: usize = 32_768;
pub const ORCHESTRATION_CHILD_DEFAULT: u32 = 64;
pub const ORCHESTRATION_CHILD_MAX: u32 = 256;
pub const ORCHESTRATION_CONCURRENCY_MAX: u8 = 4;
pub const ORCHESTRATION_RETURN_DEFAULT_BYTES: u32 = 32 * 1024;
pub const ORCHESTRATION_RETURN_MAX_BYTES: u32 = 64 * 1024;

pub const ORCHESTRATION_SCRIPT_EXTENSION: &str = "orchestration_script_v1";
pub const ORCHESTRATION_CALL_EXTENSION: &str = "orchestration_call_v1";
pub const ORCHESTRATION_CHECKPOINT_EXTENSION: &str = "orchestration_checkpoint_v1";
pub const ORCHESTRATION_TERMINAL_EXTENSION: &str = "orchestration_terminal_v1";

pub const DEFINITION_EVIDENCE_TYPES: &[&str] = &[
    "OrchValueV1",
    "OrchArgumentV1",
    "OrchRetryPolicyV1",
    "OrchAskPauseV1",
    "OrchCallV1",
    "OrchAwaitV1",
    "OrchBranchV1",
    "OrchJoinV1",
    "OrchExitV1",
];

pub const EXECUTION_EVIDENCE_TYPES: &[&str] = &[
    "OrchScriptV1",
    "OrchActivationV1",
    "OrchResultV1",
    "OrchCheckpointV1",
    "OrchTerminalV1",
];

/// A JSON value decoded with recursive duplicate-key rejection.  Keeping this
/// wrapper on open value positions prevents serde_json's map representation
/// from silently retaining only the last duplicate before typed validation.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct StrictJson(pub Value);

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictVisitor;

        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = Value;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(Value::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(Value::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(Value::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let _ = value;
                Err(E::custom("floating-point JSON numbers are not permitted"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Value::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(Value::String(value))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                StrictJson::deserialize(deserializer).map(|value| value.0)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictJson>()? {
                    values.push(value.0);
                }
                Ok(Value::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, StrictJson>()? {
                    if values.insert(key.clone(), value.0).is_some() {
                        return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
                    }
                }
                Ok(Value::Object(values))
            }
        }

        deserializer.deserialize_any(StrictVisitor).map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptRequestV1 {
    pub version: u32,
    pub transport: String,
    pub catalog_digest: String,
    pub graph: ScriptGraphV1,
    #[serde(default)]
    pub inputs: Vec<StrictJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<OrchestrationLimitsV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptGraphV1 {
    Inline {
        #[serde(default)]
        parameters: Vec<OrchTypeV1>,
        nodes: Vec<InlineNodeV1>,
        exits: Vec<u32>,
        #[serde(default)]
        read_groups: Vec<ReadGroupV1>,
    },
    Ref {
        root: InstructEvidenceRef,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationLimitsV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_nodes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_entries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_wall_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_wall_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_progress_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned_bytes: Option<u32>,
}

impl Default for OrchestrationLimitsV1 {
    fn default() -> Self {
        Self {
            definition_nodes: Some(ORCHESTRATION_NODE_DEFAULT as u32),
            parent_entries: Some(ORCHESTRATION_EDGE_DEFAULT as u32),
            child_attempts: Some(ORCHESTRATION_CHILD_DEFAULT),
            script_wall_ms: Some(120_000),
            child_wall_ms: Some(30_000),
            no_progress_ms: Some(30_000),
            returned_bytes: Some(ORCHESTRATION_RETURN_DEFAULT_BYTES),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineNodeV1 {
    pub slot: u32,
    pub evidence_type: String,
    pub config: StrictJson,
    #[serde(default)]
    pub ports: Vec<InlinePortV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlinePortV1 {
    pub role: OrchPortRoleV1,
    pub port: String,
    pub source_slot: u32,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchPortRoleV1 {
    Data,
    Control,
    Guard,
    Alternative,
    Config,
    Index,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadGroupV1 {
    pub id: String,
    pub members: Vec<u32>,
    pub max_concurrency: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegionStepV1 {
    pub branch_slot: u32,
    pub case: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OrchTypeV1 {
    Null,
    Bool,
    I64 {
        min: i64,
        max: i64,
    },
    String {
        max_bytes: u32,
    },
    List {
        item: Box<Self>,
        max_items: u32,
    },
    Record {
        fields: BTreeMap<String, Self>,
    },
    Union {
        discriminant: String,
        variants: BTreeMap<String, Self>,
    },
    Opaque {
        ref_kind: String,
        issuer_version: String,
        decoder_version: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchShapeV1 {
    pub version: u32,
    pub transport: String,
    pub codec: String,
    pub semantics: String,
    pub catalog_digest: String,
    pub parameters: Vec<OrchTypeV1>,
    pub definition_count: u32,
    pub indexes: Vec<InstructEvidenceRef>,
    pub exits: Vec<u32>,
    pub read_groups: Vec<ReadGroupV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchIndexEntryV1 {
    pub slot: u32,
    #[serde(rename = "ref")]
    pub evidence_ref: InstructEvidenceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchIndexDomainV1 {
    Definition,
    Execution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchIndexV1 {
    pub version: u32,
    pub domain: OrchIndexDomainV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_id: Option<String>,
    pub entries: Vec<OrchIndexEntryV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedPortV1 {
    pub role: OrchPortRoleV1,
    pub port: String,
    pub parent: ArtifactRef,
    pub parent_ledger_digest: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedDefinitionV1 {
    pub version: u32,
    pub evidence_type: String,
    pub slot: u32,
    pub config: StrictJson,
    pub ports: Vec<MaterializedPortV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptTerminalStatusV1 {
    Completed,
    NeedsModel,
    Failed,
    Rejected,
    Cancelled,
    TimedOut,
    Interrupted,
    OutcomeUnknown,
}

impl ScriptTerminalStatusV1 {
    #[must_use]
    pub const fn outer_status(self) -> ToolResultStatus {
        match self {
            Self::Completed | Self::NeedsModel => ToolResultStatus::Completed,
            Self::Rejected => ToolResultStatus::Rejected,
            Self::Cancelled => ToolResultStatus::Cancelled,
            Self::OutcomeUnknown => ToolResultStatus::Unknown,
            Self::Failed | Self::TimedOut | Self::Interrupted => ToolResultStatus::Failed,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptCountsV1 {
    pub calls: u32,
    pub attempts: u32,
    pub completed: u32,
    pub failed: u32,
    pub rejected: u32,
    pub cancelled: u32,
    pub unknown: u32,
    pub inactive: u32,
    pub skipped: u32,
    pub unresolved_guard: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptTerminalV1 {
    pub version: u32,
    pub script_id: String,
    pub status: ScriptTerminalStatusV1,
    pub request_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_checkpoint: Option<InstructEvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_ref: Option<InstructEvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_exit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<StrictJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub counts: ScriptCountsV1,
    #[serde(default)]
    pub receipt_refs: Vec<InstructEvidenceRef>,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationUsageV1 {
    pub scripts: u64,
    pub generated_source_bytes: u64,
    pub generated_source_tokens: u64,
    pub child_attempts: u64,
    pub retries: u64,
    pub canonical_bytes: u64,
    pub unique_cas_bytes: u64,
    pub admission_us: u64,
    pub scheduling_us: u64,
    pub wall_ms: u64,
}

impl OrchestrationUsageV1 {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.scripts == 0
            && self.generated_source_bytes == 0
            && self.generated_source_tokens == 0
            && self.child_attempts == 0
            && self.retries == 0
            && self.canonical_bytes == 0
            && self.unique_cas_bytes == 0
            && self.admission_us == 0
            && self.scheduling_us == 0
            && self.wall_ms == 0
    }
}

/// Attribution carried on the physical provider request that generated an
/// orchestration submission. Child operations never create provider requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationRequestAttributionV1 {
    pub transport: String,
    pub generated_source_bytes: u64,
    pub generated_source_tokens: u64,
    #[serde(default)]
    pub token_basis: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer_id: Option<String>,
    #[serde(default)]
    pub generating_request_ordinal: u64,
    pub graph_mode: OrchestrationGraphModeV1,
    pub cold_schema_discovery: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationGraphModeV1 {
    Inline,
    Ref,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationRunDigestV1 {
    pub script_id: String,
    pub status: ScriptTerminalStatusV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_ref: Option<InstructEvidenceRef>,
    pub counts: ScriptCountsV1,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedInlineDagV1 {
    pub parameters: Vec<OrchTypeV1>,
    pub nodes: Vec<InlineNodeV1>,
    pub exits: Vec<u32>,
    pub read_groups: Vec<ReadGroupV1>,
    pub parent_entries: usize,
    pub call_attempt_bound: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestrationError {
    pub code: &'static str,
    pub message: String,
    pub slot: Option<u32>,
}

impl fmt::Display for OrchestrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.slot {
            Some(slot) => write!(formatter, "{} at slot {slot}: {}", self.code, self.message),
            None => write!(formatter, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for OrchestrationError {}

fn invalid(code: &'static str, message: impl Into<String>) -> OrchestrationError {
    OrchestrationError {
        code,
        message: message.into(),
        slot: None,
    }
}

fn invalid_slot(slot: u32, code: &'static str, message: impl Into<String>) -> OrchestrationError {
    OrchestrationError {
        code,
        message: message.into(),
        slot: Some(slot),
    }
}

/// Parses the complete buffered request.  Callers must enforce the streaming
/// cap before invoking this function; the redundant length check protects
/// non-streaming and test callers.
pub fn parse_script_request(bytes: &[u8]) -> Result<ScriptRequestV1, OrchestrationError> {
    if bytes.is_empty() || bytes.len() > ORCHESTRATION_RAW_MAX_BYTES {
        return Err(invalid(
            "submission_size",
            format!("submission must contain 1..={ORCHESTRATION_RAW_MAX_BYTES} bytes"),
        ));
    }
    let request: ScriptRequestV1 = serde_json::from_slice(bytes)
        .map_err(|error| invalid("invalid_json", error.to_string()))?;
    if request.version != ORCHESTRATION_VERSION {
        return Err(invalid(
            "unsupported_version",
            format!("expected version {ORCHESTRATION_VERSION}"),
        ));
    }
    if request.transport != ORCHESTRATION_TRANSPORT {
        return Err(invalid(
            "unsupported_transport",
            format!("expected transport {ORCHESTRATION_TRANSPORT}"),
        ));
    }
    if request.catalog_digest.len() != 71
        || !request.catalog_digest.starts_with("blake3:")
        || !request.catalog_digest[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid("catalog_digest", "invalid catalog digest"));
    }
    validate_limits(request.limits.as_ref())?;
    if request.inputs.len() > 256 {
        return Err(invalid("input_count", "at most 256 inputs are permitted"));
    }
    Ok(request)
}

fn validate_limits(limits: Option<&OrchestrationLimitsV1>) -> Result<(), OrchestrationError> {
    let Some(limits) = limits else { return Ok(()) };
    let positive = |value: Option<u64>, max: u64, name: &'static str| {
        if value.is_some_and(|value| value == 0 || value > max) {
            Err(invalid("limit", format!("{name} must be within 1..={max}")))
        } else {
            Ok(())
        }
    };
    positive(
        limits.definition_nodes.map(u64::from),
        ORCHESTRATION_NODE_MAX as u64,
        "definition_nodes",
    )?;
    positive(
        limits.parent_entries.map(u64::from),
        ORCHESTRATION_EDGE_MAX as u64,
        "parent_entries",
    )?;
    positive(
        limits.child_attempts.map(u64::from),
        ORCHESTRATION_CHILD_MAX as u64,
        "child_attempts",
    )?;
    positive(limits.script_wall_ms, 300_000, "script_wall_ms")?;
    positive(limits.child_wall_ms, 120_000, "child_wall_ms")?;
    positive(limits.no_progress_ms, 60_000, "no_progress_ms")?;
    positive(
        limits.returned_bytes.map(u64::from),
        ORCHESTRATION_RETURN_MAX_BYTES as u64,
        "returned_bytes",
    )
}

/// Validates graph structure independent of the entry-time wrapper catalog.
/// Wrapper schema, authority, and native parser validation remain daemon
/// admission responsibilities.
pub fn validate_inline_dag(
    parameters: &[OrchTypeV1],
    nodes: &[InlineNodeV1],
    exits: &[u32],
    read_groups: &[ReadGroupV1],
    limits: Option<&OrchestrationLimitsV1>,
) -> Result<ValidatedInlineDagV1, OrchestrationError> {
    let node_limit = limits
        .and_then(|limits| limits.definition_nodes)
        .unwrap_or(ORCHESTRATION_NODE_DEFAULT as u32) as usize;
    let edge_limit = limits
        .and_then(|limits| limits.parent_entries)
        .unwrap_or(ORCHESTRATION_EDGE_DEFAULT as u32) as usize;
    let child_limit = limits
        .and_then(|limits| limits.child_attempts)
        .unwrap_or(ORCHESTRATION_CHILD_DEFAULT);
    if nodes.is_empty() || nodes.len() > node_limit {
        return Err(invalid(
            "node_count",
            format!("graph must contain 1..={node_limit} definitions"),
        ));
    }
    if parameters.len() > 256 {
        return Err(invalid(
            "parameter_count",
            "at most 256 parameters are permitted",
        ));
    }
    for parameter in parameters {
        validate_type(parameter, 0)?;
        if type_contains_opaque(parameter) {
            return Err(invalid(
                "opaque_input_provenance",
                "opaque parameters require a daemon-issued ref binding, which v1 inline inputs do not provide",
            ));
        }
    }
    let mut parent_entries = 0usize;
    let mut attempt_bound = 0u32;
    let mut reachable = BTreeSet::new();
    let mut retry_attempts = BTreeMap::new();
    for (expected, node) in nodes.iter().enumerate() {
        if node.slot as usize != expected {
            return Err(invalid_slot(
                node.slot,
                "non_dense_slot",
                format!("expected dense slot {expected}"),
            ));
        }
        if !DEFINITION_EVIDENCE_TYPES.contains(&node.evidence_type.as_str()) {
            return Err(invalid_slot(
                node.slot,
                "unknown_evidence_type",
                format!("unsupported definition type {}", node.evidence_type),
            ));
        }
        parent_entries = parent_entries
            .checked_add(node.ports.len())
            .ok_or_else(|| invalid("resource_overflow", "parent count overflow"))?;
        if parent_entries > edge_limit {
            return Err(invalid(
                "parent_count",
                format!("graph exceeds {edge_limit} parent entries"),
            ));
        }
        validate_ports(node)?;
        validate_config(node, parameters, &mut retry_attempts)?;
    }
    validate_graph_semantics(nodes, read_groups)?;
    if exits.is_empty() {
        return Err(invalid("exit_count", "at least one exit is required"));
    }
    let mut unique_exits = BTreeSet::new();
    for exit in exits {
        let node = nodes
            .get(*exit as usize)
            .ok_or_else(|| invalid("exit_slot", format!("exit slot {exit} does not exist")))?;
        if node.evidence_type != "OrchExitV1" || !unique_exits.insert(*exit) {
            return Err(invalid(
                "exit_slot",
                format!("slot {exit} is not a unique OrchExitV1"),
            ));
        }
        mark_reachable(*exit, nodes, &mut reachable);
    }
    if reachable.len() != nodes.len() {
        let slot = (0..nodes.len())
            .find(|slot| !reachable.contains(&(*slot as u32)))
            .unwrap_or_default() as u32;
        return Err(invalid_slot(
            slot,
            "disconnected_definition",
            "definition does not contribute to an exit",
        ));
    }
    validate_call_families(nodes, &retry_attempts, &mut attempt_bound)?;
    if attempt_bound > child_limit {
        return Err(invalid(
            "child_attempt_count",
            format!("worst-case {attempt_bound} attempts exceeds {child_limit}"),
        ));
    }
    validate_read_groups(nodes, read_groups)?;
    Ok(ValidatedInlineDagV1 {
        parameters: parameters.to_vec(),
        nodes: nodes.to_vec(),
        exits: exits.to_vec(),
        read_groups: read_groups.to_vec(),
        parent_entries,
        call_attempt_bound: attempt_bound,
    })
}

fn validate_type(value: &OrchTypeV1, depth: usize) -> Result<(), OrchestrationError> {
    if depth > 32 {
        return Err(invalid("type_depth", "type nesting exceeds 32"));
    }
    match value {
        OrchTypeV1::I64 { min, max } if min > max => {
            Err(invalid("type_bounds", "i64 min exceeds max"))
        }
        OrchTypeV1::String { max_bytes } if *max_bytes == 0 || *max_bytes > 1_048_576 => Err(
            invalid("type_bounds", "string max_bytes is outside 1..=1 MiB"),
        ),
        OrchTypeV1::List { item, max_items } => {
            if *max_items > 4_096 {
                return Err(invalid("type_bounds", "list max_items exceeds 4096"));
            }
            validate_type(item, depth + 1)
        }
        OrchTypeV1::Record { fields } => {
            if fields.len() > 256 {
                return Err(invalid("type_bounds", "record has more than 256 fields"));
            }
            validate_named_types(fields, depth)
        }
        OrchTypeV1::Union {
            discriminant,
            variants,
        } => {
            validate_name(discriminant, "union discriminant")?;
            if variants.is_empty() || variants.len() > 256 {
                return Err(invalid("type_bounds", "union must have 1..=256 variants"));
            }
            validate_named_types(variants, depth)?;
            for (tag, variant) in variants {
                let OrchTypeV1::Record { fields } = variant else {
                    return Err(invalid(
                        "type_bounds",
                        "union variants must be closed records containing the discriminant",
                    ));
                };
                let Some(OrchTypeV1::String { max_bytes }) = fields.get(discriminant) else {
                    return Err(invalid(
                        "type_bounds",
                        "union variant is missing its string discriminant field",
                    ));
                };
                if tag.len() > *max_bytes as usize {
                    return Err(invalid(
                        "type_bounds",
                        "union tag exceeds its discriminant string bound",
                    ));
                }
            }
            Ok(())
        }
        OrchTypeV1::Opaque {
            ref_kind,
            issuer_version,
            decoder_version,
        } => {
            validate_name(ref_kind, "opaque ref_kind")?;
            validate_name(issuer_version, "opaque issuer_version")?;
            validate_name(decoder_version, "opaque decoder_version")
        }
        _ => Ok(()),
    }
}

fn validate_typed_value(
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
                validate_typed_value(value, item, depth + 1)?;
            }
            Ok(())
        }
        OrchTypeV1::Record { fields } => {
            let record = value
                .as_object()
                .filter(|record| record.len() == fields.len())
                .ok_or("value is not the declared closed record")?;
            for (field, field_type) in fields {
                validate_typed_value(
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
            validate_typed_value(value, variant, depth + 1)
        }
        // Opaque values are daemon-issued capabilities. Shape is checked here;
        // issuer provenance is checked when an admitted input is resolved.
        OrchTypeV1::Opaque { .. } if value.is_object() => Ok(()),
        _ => Err("value does not match its declared type".into()),
    }
}

fn type_contains_opaque(value: &OrchTypeV1) -> bool {
    match value {
        OrchTypeV1::Opaque { .. } => true,
        OrchTypeV1::List { item, .. } => type_contains_opaque(item),
        OrchTypeV1::Record { fields } => fields.values().any(type_contains_opaque),
        OrchTypeV1::Union { variants, .. } => variants.values().any(type_contains_opaque),
        OrchTypeV1::Null
        | OrchTypeV1::Bool
        | OrchTypeV1::I64 { .. }
        | OrchTypeV1::String { .. } => false,
    }
}

fn validate_named_types(
    fields: &BTreeMap<String, OrchTypeV1>,
    depth: usize,
) -> Result<(), OrchestrationError> {
    for (name, value) in fields {
        validate_name(name, "type field")?;
        validate_type(value, depth + 1)?;
    }
    Ok(())
}

fn validate_name(name: &str, what: &str) -> Result<(), OrchestrationError> {
    if name.is_empty() || name.len() > 64 {
        Err(invalid(
            "name_bounds",
            format!("{what} must contain 1..=64 UTF-8 bytes"),
        ))
    } else {
        Ok(())
    }
}

fn validate_ports(node: &InlineNodeV1) -> Result<(), OrchestrationError> {
    if node.ports.len() > 4_096 {
        return Err(invalid_slot(
            node.slot,
            "port_count",
            "more than 4096 ports",
        ));
    }
    let mut names = HashSet::new();
    let mut last_rank = 0u8;
    for (index, port) in node.ports.iter().enumerate() {
        validate_name(&port.port, "port name").map_err(|mut error| {
            error.slot = Some(node.slot);
            error
        })?;
        validate_name(&port.output, "output name").map_err(|mut error| {
            error.slot = Some(node.slot);
            error
        })?;
        if !names.insert(port.port.as_str()) {
            return Err(invalid_slot(node.slot, "duplicate_port", &port.port));
        }
        if port.source_slot >= node.slot {
            return Err(invalid_slot(
                node.slot,
                "non_topological_edge",
                format!("port {index} references slot {}", port.source_slot),
            ));
        }
        let rank = match port.role {
            OrchPortRoleV1::Data => 0,
            OrchPortRoleV1::Control => 1,
            OrchPortRoleV1::Config => 2,
            OrchPortRoleV1::Alternative => 3,
            OrchPortRoleV1::Index => 4,
            OrchPortRoleV1::Guard => 5,
        };
        if index > 0 && rank < last_rank {
            return Err(invalid_slot(
                node.slot,
                "port_order",
                "ports are not in canonical role order",
            ));
        }
        last_rank = rank;
        let selector_required = matches!(
            port.role,
            OrchPortRoleV1::Guard | OrchPortRoleV1::Alternative
        );
        if selector_required != port.selector.is_some() {
            return Err(invalid_slot(
                node.slot,
                "port_selector",
                "selector presence does not match the port role",
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryConfig {
    #[serde(default = "one")]
    max_attempts: u8,
}

const fn one() -> u8 {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolConfig {
    tool: String,
    wrapper_digest: String,
    #[serde(default)]
    region: Vec<RegionStepV1>,
    #[serde(default)]
    origin: Option<Vec<u32>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskConfig {
    tool: String,
    wrapper_digest: String,
    owner_call_slot: u32,
    #[serde(default = "default_ask_wait")]
    wait_ms: u64,
    #[serde(default)]
    region: Vec<RegionStepV1>,
    #[serde(default)]
    origin: Option<Vec<u32>>,
}

const fn default_ask_wait() -> u64 {
    120_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AwaitConfig {
    #[serde(default)]
    on_error: AwaitErrorMode,
    #[serde(default)]
    region: Vec<RegionStepV1>,
    #[serde(default)]
    origin: Option<Vec<u32>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AwaitErrorMode {
    #[default]
    Stop,
    Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueConfig {
    operator: ValueOperator,
    #[serde(rename = "type")]
    value_type: OrchTypeV1,
    operand_config: StrictJson,
    #[serde(default)]
    region: Vec<RegionStepV1>,
    #[serde(default)]
    origin: Option<Vec<u32>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ValueOperator {
    Literal,
    Input,
    Get,
    Index,
    Record,
    List,
    Builtin,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchConfig {
    cases: Vec<String>,
    #[serde(default)]
    region: Vec<RegionStepV1>,
    #[serde(default)]
    origin: Option<Vec<u32>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinConfig {
    mode: JoinMode,
    #[serde(rename = "type")]
    value_type: OrchTypeV1,
    #[serde(default)]
    omit_inactive: bool,
    #[serde(default)]
    region: Vec<RegionStepV1>,
    #[serde(default)]
    origin: Option<Vec<u32>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum JoinMode {
    All,
    Select,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitConfig {
    mode: ExitMode,
    #[serde(default)]
    region: Vec<RegionStepV1>,
    #[serde(default)]
    origin: Option<Vec<u32>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExitMode {
    Return,
    Fail,
    NeedModel,
}

fn decode_config<T: for<'de> Deserialize<'de>>(
    node: &InlineNodeV1,
) -> Result<T, OrchestrationError> {
    serde_json::from_value(node.config.0.clone())
        .map_err(|error| invalid_slot(node.slot, "invalid_config", error.to_string()))
}

fn validate_config(
    node: &InlineNodeV1,
    parameters: &[OrchTypeV1],
    retries: &mut BTreeMap<u32, u8>,
) -> Result<(), OrchestrationError> {
    let validate_region = |region: &[RegionStepV1], origin: Option<&Vec<u32>>| {
        if region.len() > 32 || origin.is_some_and(|origin| origin.len() > 32) {
            Err(invalid_slot(
                node.slot,
                "region_depth",
                "region/origin depth exceeds 32",
            ))
        } else if region.iter().any(|step| {
            step.branch_slot >= node.slot || step.case.is_empty() || step.case.len() > 64
        }) {
            Err(invalid_slot(
                node.slot,
                "region",
                "invalid enclosing branch coordinate",
            ))
        } else {
            Ok(())
        }
    };
    match node.evidence_type.as_str() {
        "OrchRetryPolicyV1" => {
            let config: RetryConfig = decode_config(node)?;
            if !(1..=3).contains(&config.max_attempts) {
                return Err(invalid_slot(
                    node.slot,
                    "retry_attempts",
                    "max_attempts must be 1, 2, or 3",
                ));
            }
            retries.insert(node.slot, config.max_attempts);
        }
        "OrchArgumentV1" | "OrchCallV1" => {
            let config: ToolConfig = decode_config(node)?;
            validate_tool_config(node, &config)?;
            validate_region(&config.region, config.origin.as_ref())?;
        }
        "OrchAskPauseV1" => {
            let config: AskConfig = decode_config(node)?;
            validate_name(&config.tool, "tool name")?;
            validate_digest(&config.wrapper_digest, node.slot, "wrapper_digest")?;
            if config.owner_call_slot <= node.slot
                || config.wait_ms == 0
                || config.wait_ms > 300_000
            {
                return Err(invalid_slot(
                    node.slot,
                    "ask_config",
                    "invalid owner_call_slot or wait_ms",
                ));
            }
            validate_region(&config.region, config.origin.as_ref())?;
        }
        "OrchAwaitV1" => {
            let config: AwaitConfig = decode_config(node)?;
            let _ = config.on_error;
            validate_region(&config.region, config.origin.as_ref())?;
        }
        "OrchValueV1" => {
            let config: ValueConfig = decode_config(node)?;
            validate_type(&config.value_type, 0)?;
            validate_value_operator(node, &config, parameters)?;
            validate_region(&config.region, config.origin.as_ref())?;
        }
        "OrchBranchV1" => {
            let config: BranchConfig = decode_config(node)?;
            if config.cases.is_empty() || config.cases.len() > 256 {
                return Err(invalid_slot(
                    node.slot,
                    "branch_cases",
                    "branch must have 1..=256 cases",
                ));
            }
            let mut cases = HashSet::new();
            for case in &config.cases {
                validate_name(case, "branch case")?;
                if !cases.insert(case) {
                    return Err(invalid_slot(
                        node.slot,
                        "branch_cases",
                        "duplicate branch case",
                    ));
                }
            }
            validate_region(&config.region, config.origin.as_ref())?;
        }
        "OrchJoinV1" => {
            let config: JoinConfig = decode_config(node)?;
            validate_type(&config.value_type, 0)?;
            let _ = config.mode;
            validate_region(&config.region, config.origin.as_ref())?;
        }
        "OrchExitV1" => {
            let config: ExitConfig = decode_config(node)?;
            let _ = config.mode;
            validate_region(&config.region, config.origin.as_ref())?;
        }
        _ => unreachable!("registry checked before config"),
    }
    Ok(())
}

fn validate_tool_config(
    node: &InlineNodeV1,
    config: &ToolConfig,
) -> Result<(), OrchestrationError> {
    validate_name(&config.tool, "tool name").map_err(|mut error| {
        error.slot = Some(node.slot);
        error
    })?;
    if matches!(
        config.tool.as_str(),
        "tool_script" | "task_outcome" | "spawn_subagent" | "send_input" | "wait" | "close_agent"
    ) {
        return Err(invalid_slot(
            node.slot,
            "lifecycle_tool",
            format!("{} is not script-bindable", config.tool),
        ));
    }
    validate_digest(&config.wrapper_digest, node.slot, "wrapper_digest")
}

fn validate_digest(value: &str, slot: u32, name: &str) -> Result<(), OrchestrationError> {
    if value.len() == 71
        && value.starts_with("blake3:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(invalid_slot(slot, "digest", format!("invalid {name}")))
    }
}

fn validate_value_operator(
    node: &InlineNodeV1,
    config: &ValueConfig,
    parameters: &[OrchTypeV1],
) -> Result<(), OrchestrationError> {
    let object = config.operand_config.0.as_object().ok_or_else(|| {
        invalid_slot(
            node.slot,
            "operand_config",
            "operand_config must be an object",
        )
    })?;
    match config.operator {
        ValueOperator::Literal => {
            if object.len() != 1 || !object.contains_key("value") {
                return Err(invalid_slot(
                    node.slot,
                    "operand_config",
                    "literal requires only value",
                ));
            }
            if type_contains_opaque(&config.value_type) {
                return Err(invalid_slot(
                    node.slot,
                    "opaque_literal_provenance",
                    "opaque capabilities cannot be constructed by a literal",
                ));
            }
            validate_typed_value(&object["value"], &config.value_type, 0)
                .map_err(|message| invalid_slot(node.slot, "literal_type", message))?;
        }
        ValueOperator::Input => {
            let parameter = object
                .get("parameter")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    invalid_slot(
                        node.slot,
                        "operand_config",
                        "input requires unsigned parameter",
                    )
                })?;
            if object.len() != 1 || parameter as usize >= parameters.len() {
                return Err(invalid_slot(
                    node.slot,
                    "operand_config",
                    "input parameter is not declared",
                ));
            }
            if parameters.get(parameter as usize) != Some(&config.value_type) {
                return Err(invalid_slot(
                    node.slot,
                    "input_type",
                    "input value type does not match its parameter descriptor",
                ));
            }
        }
        ValueOperator::Get => {
            if object.len() != 1 || object.get("field").and_then(Value::as_str).is_none() {
                return Err(invalid_slot(
                    node.slot,
                    "operand_config",
                    "get requires only field",
                ));
            }
        }
        ValueOperator::Index | ValueOperator::List => {
            if !object.is_empty() {
                return Err(invalid_slot(
                    node.slot,
                    "operand_config",
                    "operator config must be empty",
                ));
            }
        }
        ValueOperator::Record => {
            let fields = object
                .get("fields")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    invalid_slot(node.slot, "operand_config", "record requires fields")
                })?;
            if object.len() != 1
                || fields.len() > 256
                || fields.iter().any(|field| {
                    field
                        .as_str()
                        .is_none_or(|field| field.is_empty() || field.len() > 64)
                })
            {
                return Err(invalid_slot(
                    node.slot,
                    "operand_config",
                    "invalid record fields",
                ));
            }
        }
        ValueOperator::Builtin => {
            const BUILTINS: &[&str] = &[
                "add",
                "sub",
                "mul",
                "div",
                "rem",
                "eq",
                "lt",
                "le",
                "gt",
                "ge",
                "and",
                "or",
                "not",
                "len",
                "sha256",
                "concat",
                "slice",
                "split",
                "repeat",
                "parse_json",
            ];
            let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
                invalid_slot(node.slot, "operand_config", "builtin requires name")
            })?;
            if object.len() != 1 || !BUILTINS.contains(&name) {
                return Err(invalid_slot(node.slot, "operand_config", "unknown builtin"));
            }
        }
    }
    Ok(())
}

fn mark_reachable(slot: u32, nodes: &[InlineNodeV1], reachable: &mut BTreeSet<u32>) {
    if !reachable.insert(slot) {
        return;
    }
    if let Some(node) = nodes.get(slot as usize) {
        for port in &node.ports {
            mark_reachable(port.source_slot, nodes, reachable);
        }
    }
}

fn port<'a>(node: &'a InlineNodeV1, role: OrchPortRoleV1, name: &str) -> Option<&'a InlinePortV1> {
    node.ports
        .iter()
        .find(|port| port.role == role && port.port == name)
}

fn validate_call_families(
    nodes: &[InlineNodeV1],
    retries: &BTreeMap<u32, u8>,
    attempts: &mut u32,
) -> Result<(), OrchestrationError> {
    let mut awaited = HashSet::new();
    for node in nodes
        .iter()
        .filter(|node| node.evidence_type == "OrchCallV1")
    {
        let args = port(node, OrchPortRoleV1::Data, "args")
            .ok_or_else(|| invalid_slot(node.slot, "call_ports", "call requires data:args"))?;
        let permit = port(node, OrchPortRoleV1::Control, "permit")
            .ok_or_else(|| invalid_slot(node.slot, "call_ports", "call requires control:permit"))?;
        let retry = port(node, OrchPortRoleV1::Config, "retry")
            .ok_or_else(|| invalid_slot(node.slot, "call_ports", "call requires config:retry"))?;
        let argument = &nodes[args.source_slot as usize];
        let ask = &nodes[permit.source_slot as usize];
        if argument.evidence_type != "OrchArgumentV1"
            || ask.evidence_type != "OrchAskPauseV1"
            || nodes[retry.source_slot as usize].evidence_type != "OrchRetryPolicyV1"
        {
            return Err(invalid_slot(
                node.slot,
                "call_family",
                "call argument/ask/retry types do not match",
            ));
        }
        let call_config: ToolConfig = decode_config(node)?;
        let arg_config: ToolConfig = decode_config(argument)?;
        let ask_config: AskConfig = decode_config(ask)?;
        if call_config.tool != arg_config.tool
            || call_config.tool != ask_config.tool
            || call_config.wrapper_digest != arg_config.wrapper_digest
            || call_config.wrapper_digest != ask_config.wrapper_digest
            || ask_config.owner_call_slot != node.slot
            || port(ask, OrchPortRoleV1::Data, "args")
                .is_none_or(|ask_args| ask_args.source_slot != args.source_slot)
        {
            return Err(invalid_slot(
                node.slot,
                "call_family",
                "argument/ask/call identity does not match",
            ));
        }
        let matching_awaits = nodes
            .iter()
            .filter(|candidate| {
                candidate.evidence_type == "OrchAwaitV1"
                    && port(candidate, OrchPortRoleV1::Data, "operation")
                        .is_some_and(|operation| operation.source_slot == node.slot)
            })
            .collect::<Vec<_>>();
        if matching_awaits.len() != 1 || !awaited.insert(node.slot) {
            return Err(invalid_slot(
                node.slot,
                "await_ownership",
                "call must have exactly one Await",
            ));
        }
        let await_config: AwaitConfig = decode_config(matching_awaits[0])?;
        if call_config.region != arg_config.region
            || call_config.region != ask_config.region
            || call_config.region != await_config.region
        {
            return Err(invalid_slot(
                node.slot,
                "call_region",
                "Argument/Ask/Call/Await regions must be identical",
            ));
        }
        *attempts = attempts
            .checked_add(u32::from(*retries.get(&retry.source_slot).unwrap_or(&1)))
            .ok_or_else(|| invalid("resource_overflow", "attempt count overflow"))?;
    }
    Ok(())
}

fn validate_graph_semantics(
    nodes: &[InlineNodeV1],
    read_groups: &[ReadGroupV1],
) -> Result<(), OrchestrationError> {
    for node in nodes {
        if node
            .ports
            .iter()
            .any(|port| port.role == OrchPortRoleV1::Index)
        {
            return Err(invalid_slot(
                node.slot,
                "index_port",
                "inline definitions cannot contain index ports",
            ));
        }
        validate_node_port_shape(node, nodes)?;
        validate_region_guards(node, nodes)?;
        for port in &node.ports {
            let producer = &nodes[port.source_slot as usize];
            validate_producer_output(node, port, producer)?;
            validate_region_flow(node, port, producer)?;
        }
    }
    validate_branch_coverage(nodes)?;
    validate_static_types(nodes)?;
    validate_call_ordering(nodes, read_groups)
}

fn validate_static_types(nodes: &[InlineNodeV1]) -> Result<(), OrchestrationError> {
    let mut outputs = Vec::<Option<OrchTypeV1>>::with_capacity(nodes.len());
    for node in nodes {
        let output = match node.evidence_type.as_str() {
            "OrchValueV1" => {
                let config: ValueConfig = decode_config(node)?;
                validate_value_types(node, &config, &outputs)?;
                Some(config.value_type)
            }
            "OrchArgumentV1" => Some(data_source_type(node, "args", &outputs)?.clone()),
            "OrchAwaitV1" => Some(tool_result_type()),
            "OrchBranchV1" => {
                let subject = data_source_type(node, "subject", &outputs)?;
                if !matches!(subject, OrchTypeV1::Bool | OrchTypeV1::Union { .. })
                    && nodes[port(node, OrchPortRoleV1::Data, "subject")
                        .expect("port shape validated")
                        .source_slot as usize]
                        .evidence_type
                        != "OrchAwaitV1"
                {
                    return Err(type_error(
                        node,
                        "branch subject must be bool, a finite tagged union, or an Await result",
                    ));
                }
                None
            }
            "OrchJoinV1" => {
                let config: JoinConfig = decode_config(node)?;
                validate_join_types(node, &config, nodes, &outputs)?;
                Some(config.value_type)
            }
            "OrchRetryPolicyV1" | "OrchAskPauseV1" | "OrchCallV1" | "OrchExitV1" => None,
            _ => unreachable!("definition registry checked before static typing"),
        };
        outputs.push(output);
    }
    Ok(())
}

fn tool_result_type() -> OrchTypeV1 {
    OrchTypeV1::Opaque {
        ref_kind: "tool_result".into(),
        issuer_version: "1".into(),
        decoder_version: "1".into(),
    }
}

fn type_error(node: &InlineNodeV1, message: impl Into<String>) -> OrchestrationError {
    invalid_slot(node.slot, "static_type", message.into())
}

fn data_source_type<'a>(
    node: &InlineNodeV1,
    name: &str,
    outputs: &'a [Option<OrchTypeV1>],
) -> Result<&'a OrchTypeV1, OrchestrationError> {
    let source = port(node, OrchPortRoleV1::Data, name)
        .ok_or_else(|| type_error(node, format!("missing data port {name}")))?;
    outputs
        .get(source.source_slot as usize)
        .and_then(Option::as_ref)
        .ok_or_else(|| type_error(node, format!("data port {name} has no typed value output")))
}

fn ordered_input_types<'a>(
    node: &InlineNodeV1,
    role: OrchPortRoleV1,
    outputs: &'a [Option<OrchTypeV1>],
) -> Result<Vec<&'a OrchTypeV1>, OrchestrationError> {
    node.ports
        .iter()
        .filter(|port| port.role == role)
        .map(|port| {
            outputs
                .get(port.source_slot as usize)
                .and_then(Option::as_ref)
                .ok_or_else(|| {
                    type_error(
                        node,
                        format!("port {} has no typed value output", port.port),
                    )
                })
        })
        .collect()
}

fn require_exact_type(
    node: &InlineNodeV1,
    actual: &OrchTypeV1,
    expected: &OrchTypeV1,
    context: &str,
) -> Result<(), OrchestrationError> {
    if actual == expected {
        Ok(())
    } else {
        Err(type_error(
            node,
            format!("{context} has a different structural type than declared"),
        ))
    }
}

fn validate_value_types(
    node: &InlineNodeV1,
    config: &ValueConfig,
    outputs: &[Option<OrchTypeV1>],
) -> Result<(), OrchestrationError> {
    let operands = ordered_input_types(node, OrchPortRoleV1::Data, outputs)?;
    match config.operator {
        ValueOperator::Literal | ValueOperator::Input => Ok(()),
        ValueOperator::Get => {
            let field = config.operand_config.0["field"]
                .as_str()
                .expect("operator config validated");
            let source = operands[0];
            let field_type = match source {
                OrchTypeV1::Record { fields } => fields.get(field),
                OrchTypeV1::Union { variants, .. } => {
                    let mut common = None;
                    for variant in variants.values() {
                        let OrchTypeV1::Record { fields } = variant else {
                            return Err(type_error(
                                node,
                                "get requires a common union record field",
                            ));
                        };
                        let candidate = fields.get(field).ok_or_else(|| {
                            type_error(node, "get field is not present in every union variant")
                        })?;
                        if common.is_some_and(|existing| existing != candidate) {
                            return Err(type_error(
                                node,
                                "get field type differs across union variants",
                            ));
                        }
                        common = Some(candidate);
                    }
                    common
                }
                _ => {
                    return Err(type_error(
                        node,
                        "get operand is not a closed record or union",
                    ));
                }
            }
            .ok_or_else(|| type_error(node, format!("record field {field} does not exist")))?;
            require_exact_type(node, &config.value_type, field_type, "get output")
        }
        ValueOperator::Index => {
            let OrchTypeV1::List { item, .. } = operands[0] else {
                return Err(type_error(node, "index first operand is not a list"));
            };
            if !matches!(operands[1], OrchTypeV1::I64 { .. }) {
                return Err(type_error(node, "index second operand is not i64"));
            }
            require_exact_type(node, &config.value_type, item, "index output")
        }
        ValueOperator::Record => {
            let fields = match &config.value_type {
                OrchTypeV1::Record { fields } => fields,
                OrchTypeV1::Union { variants, .. } => {
                    let names = config.operand_config.0["fields"]
                        .as_array()
                        .expect("operator config validated");
                    let names = names.iter().filter_map(Value::as_str).collect::<Vec<_>>();
                    let matching = variants
                        .values()
                        .filter_map(|variant| match variant {
                            OrchTypeV1::Record { fields }
                                if fields.keys().map(String::as_str).eq(names.iter().copied()) =>
                            {
                                Some(fields)
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    if matching.len() != 1 {
                        return Err(type_error(
                            node,
                            "union record operator must match exactly one closed variant",
                        ));
                    }
                    matching[0]
                }
                _ => return Err(type_error(node, "record operator output is not a record")),
            };
            let names = config.operand_config.0["fields"]
                .as_array()
                .expect("operator config validated");
            if fields.len() != names.len() {
                return Err(type_error(
                    node,
                    "record field count differs from its output type",
                ));
            }
            for ((name, operand), (declared_name, declared_type)) in
                names.iter().zip(operands).zip(fields)
            {
                if name.as_str() != Some(declared_name) {
                    return Err(type_error(
                        node,
                        "record fields are not in canonical sorted order",
                    ));
                }
                require_exact_type(node, operand, declared_type, "record field")?;
            }
            Ok(())
        }
        ValueOperator::List => {
            let OrchTypeV1::List { item, max_items } = &config.value_type else {
                return Err(type_error(node, "list operator output is not a list"));
            };
            if operands.len() > *max_items as usize {
                return Err(type_error(
                    node,
                    "list operator exceeds its declared item bound",
                ));
            }
            for operand in operands {
                require_exact_type(node, operand, item, "list item")?;
            }
            Ok(())
        }
        ValueOperator::Builtin => validate_builtin_types(node, config, &operands),
    }
}

fn validate_builtin_types(
    node: &InlineNodeV1,
    config: &ValueConfig,
    operands: &[&OrchTypeV1],
) -> Result<(), OrchestrationError> {
    let name = config.operand_config.0["name"]
        .as_str()
        .expect("operator config validated");
    let all_i64 = || {
        operands
            .iter()
            .all(|value| matches!(value, OrchTypeV1::I64 { .. }))
    };
    let all_bool = || {
        operands
            .iter()
            .all(|value| matches!(value, OrchTypeV1::Bool))
    };
    let all_string = || {
        operands
            .iter()
            .all(|value| matches!(value, OrchTypeV1::String { .. }))
    };
    let output_i64 = matches!(config.value_type, OrchTypeV1::I64 { .. });
    let output_bool = matches!(config.value_type, OrchTypeV1::Bool);
    let output_string = matches!(config.value_type, OrchTypeV1::String { .. });
    let valid = match name {
        "add" | "sub" | "mul" | "div" | "rem" => all_i64() && output_i64,
        "lt" | "le" | "gt" | "ge" => all_i64() && output_bool,
        "eq" => operands[0] == operands[1] && output_bool,
        "and" | "or" => all_bool() && output_bool,
        "not" => all_bool() && output_bool,
        "len" => {
            matches!(
                operands[0],
                OrchTypeV1::List { .. } | OrchTypeV1::String { .. }
            ) && output_i64
        }
        "sha256" => {
            all_string()
                && matches!(config.value_type, OrchTypeV1::String { max_bytes } if max_bytes >= 64)
        }
        "concat" => all_string() && output_string,
        "slice" => {
            matches!(operands[0], OrchTypeV1::String { .. })
                && matches!(operands[1], OrchTypeV1::I64 { .. })
                && matches!(operands[2], OrchTypeV1::I64 { .. })
                && output_string
        }
        "split" => {
            all_string()
                && matches!(config.value_type, OrchTypeV1::List { ref item, .. } if matches!(item.as_ref(), OrchTypeV1::String { .. }))
        }
        "repeat" => {
            matches!(operands[0], OrchTypeV1::String { .. })
                && matches!(operands[1], OrchTypeV1::I64 { .. })
                && output_string
        }
        "parse_json" => {
            matches!(operands[0], OrchTypeV1::String { .. })
                && !type_contains_opaque(&config.value_type)
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(type_error(
            node,
            format!("builtin {name} operand/output types do not match its signature"),
        ))
    }
}

fn validate_join_types(
    node: &InlineNodeV1,
    config: &JoinConfig,
    nodes: &[InlineNodeV1],
    outputs: &[Option<OrchTypeV1>],
) -> Result<(), OrchestrationError> {
    match config.mode {
        JoinMode::Select => {
            for alternative in ordered_input_types(node, OrchPortRoleV1::Alternative, outputs)? {
                require_exact_type(node, alternative, &config.value_type, "select alternative")?;
            }
            Ok(())
        }
        JoinMode::All => {
            let operands = ordered_input_types(node, OrchPortRoleV1::Data, outputs)?;
            if operands.is_empty() {
                return if config.value_type == OrchTypeV1::Null {
                    Ok(())
                } else {
                    Err(type_error(node, "an empty all join must have null type"))
                };
            }
            let OrchTypeV1::List { item, max_items } = &config.value_type else {
                return Err(type_error(
                    node,
                    "a value-producing all join must have list type",
                ));
            };
            if operands.len() > *max_items as usize {
                return Err(type_error(node, "all join exceeds its declared list bound"));
            }
            if config.omit_inactive {
                validate_runtime_map_lowering(node, nodes, outputs, item, *max_items)?;
            } else {
                for operand in operands {
                    require_exact_type(node, operand, item, "all-join item")?;
                }
            }
            Ok(())
        }
    }
}

fn validate_runtime_map_lowering(
    node: &InlineNodeV1,
    nodes: &[InlineNodeV1],
    outputs: &[Option<OrchTypeV1>],
    item: &OrchTypeV1,
    max_items: u32,
) -> Result<(), OrchestrationError> {
    let data_ports = ports_with_role(node, OrchPortRoleV1::Data);
    if data_ports.is_empty() || data_ports.len() > 256 || data_ports.len() != max_items as usize {
        return Err(invalid_slot(
            node.slot,
            "unsupported_lowering",
            "omit_inactive requires one recognized body for every bounded-list index",
        ));
    }
    let mut source_list = None;
    for (expected_index, data_port) in data_ports.iter().enumerate() {
        let select = &nodes[data_port.source_slot as usize];
        let select_config: JoinConfig = decode_config(select)?;
        if select.evidence_type != "OrchJoinV1"
            || !matches!(select_config.mode, JoinMode::Select)
            || select_config.omit_inactive
        {
            return Err(map_lowering_error(
                node,
                "body output is not a plain select join",
            ));
        }
        let OrchTypeV1::Union {
            discriminant,
            variants,
        } = outputs[data_port.source_slot as usize]
            .as_ref()
            .ok_or_else(|| map_lowering_error(node, "body select has no typed output"))?
        else {
            return Err(map_lowering_error(
                node,
                "body select does not produce a present/absent union",
            ));
        };
        validate_presence_union(node, discriminant, variants, item)?;

        let choice = port(select, OrchPortRoleV1::Data, "choice")
            .ok_or_else(|| map_lowering_error(node, "body select has no choice"))?;
        let branch = &nodes[choice.source_slot as usize];
        if branch.evidence_type != "OrchBranchV1"
            || branch.config.0["cases"] != serde_json::json!(["true", "false"])
        {
            return Err(map_lowering_error(
                node,
                "body select is not controlled by an exact bool branch",
            ));
        }
        let condition_port = port(branch, OrchPortRoleV1::Data, "subject")
            .ok_or_else(|| map_lowering_error(node, "map branch has no condition"))?;
        let condition = &nodes[condition_port.source_slot as usize];
        if condition.evidence_type != "OrchValueV1"
            || condition.config.0["operator"].as_str() != Some("builtin")
            || condition.config.0["operand_config"]["name"].as_str() != Some("lt")
        {
            return Err(map_lowering_error(
                node,
                "map branch condition is not index < len(list)",
            ));
        }
        let condition_inputs = ports_with_role(condition, OrchPortRoleV1::Data);
        if condition_inputs.len() != 2 {
            return Err(map_lowering_error(node, "map condition arity is invalid"));
        }
        let index_node = &nodes[condition_inputs[0].source_slot as usize];
        if literal_i64(index_node) != Some(expected_index as i64) {
            return Err(map_lowering_error(
                node,
                "map indices are not contiguous constants starting at zero",
            ));
        }
        let length = &nodes[condition_inputs[1].source_slot as usize];
        if length.evidence_type != "OrchValueV1"
            || length.config.0["operator"].as_str() != Some("builtin")
            || length.config.0["operand_config"]["name"].as_str() != Some("len")
        {
            return Err(map_lowering_error(node, "map bound is not len(list)"));
        }
        let list_port = port(length, OrchPortRoleV1::Data, "value")
            .or_else(|| {
                ports_with_role(length, OrchPortRoleV1::Data)
                    .first()
                    .copied()
            })
            .ok_or_else(|| map_lowering_error(node, "len has no list operand"))?;
        let list_slot = list_port.source_slot;
        if source_list
            .replace(list_slot)
            .is_some_and(|prior| prior != list_slot)
        {
            return Err(map_lowering_error(
                node,
                "map bodies do not inspect the same immutable list",
            ));
        }
        let OrchTypeV1::List {
            item: source_item,
            max_items: source_max,
        } = outputs[list_slot as usize]
            .as_ref()
            .ok_or_else(|| map_lowering_error(node, "map source has no typed output"))?
        else {
            return Err(map_lowering_error(node, "map source is not a list"));
        };
        if source_item.as_ref() != item || *source_max != max_items {
            return Err(map_lowering_error(
                node,
                "map source bounds/item type differ from the result list",
            ));
        }

        let true_source = select
            .ports
            .iter()
            .find(|port| {
                port.role == OrchPortRoleV1::Alternative && port.selector.as_deref() == Some("true")
            })
            .map(|port| port.source_slot)
            .ok_or_else(|| map_lowering_error(node, "map body has no true producer"))?;
        let false_source = select
            .ports
            .iter()
            .find(|port| {
                port.role == OrchPortRoleV1::Alternative
                    && port.selector.as_deref() == Some("false")
            })
            .map(|port| port.source_slot)
            .ok_or_else(|| map_lowering_error(node, "map body has no false producer"))?;
        if union_constructor_tag(&nodes[true_source as usize], discriminant, nodes)
            != Some("present")
            || union_constructor_tag(&nodes[false_source as usize], discriminant, nodes)
                != Some("absent")
        {
            return Err(map_lowering_error(
                node,
                "map true/false producers do not construct matching present/absent variants",
            ));
        }
        let indexed = nodes.iter().any(|candidate| {
            candidate.slot <= true_source
                && candidate.evidence_type == "OrchValueV1"
                && candidate.config.0["operator"].as_str() == Some("index")
                && ports_with_role(candidate, OrchPortRoleV1::Data)
                    .iter()
                    .map(|port| port.source_slot)
                    .eq([list_slot, index_node.slot])
                && node_depends_on(true_source, candidate.slot, nodes)
        });
        if !indexed {
            return Err(map_lowering_error(
                node,
                "map true producer does not consume the guarded checked index",
            ));
        }
    }
    Ok(())
}

fn union_constructor_tag<'a>(
    node: &'a InlineNodeV1,
    discriminant: &str,
    nodes: &'a [InlineNodeV1],
) -> Option<&'a str> {
    if node.evidence_type != "OrchValueV1" {
        return None;
    }
    match node.config.0["operator"].as_str()? {
        "literal" => node.config.0["operand_config"]["value"][discriminant].as_str(),
        "record" => {
            let fields = node.config.0["operand_config"]["fields"].as_array()?;
            let index = fields
                .iter()
                .position(|field| field.as_str() == Some(discriminant))?;
            let source = ports_with_role(node, OrchPortRoleV1::Data)
                .get(index)?
                .source_slot;
            nodes[source as usize].config.0["operand_config"]["value"].as_str()
        }
        _ => None,
    }
}

fn validate_presence_union(
    node: &InlineNodeV1,
    discriminant: &str,
    variants: &BTreeMap<String, OrchTypeV1>,
    item: &OrchTypeV1,
) -> Result<(), OrchestrationError> {
    if discriminant != "kind"
        || variants.len() != 2
        || !variants.contains_key("present")
        || !variants.contains_key("absent")
    {
        return Err(map_lowering_error(
            node,
            "map body union variants must be exactly absent/present",
        ));
    }
    let OrchTypeV1::Record { fields: present } = &variants["present"] else {
        return Err(map_lowering_error(node, "present variant is not a record"));
    };
    let OrchTypeV1::Record { fields: absent } = &variants["absent"] else {
        return Err(map_lowering_error(node, "absent variant is not a record"));
    };
    if present.len() != 2
        || absent.len() != 1
        || present.get("value") != Some(item)
        || !present.contains_key(discriminant)
        || !absent.contains_key(discriminant)
    {
        return Err(map_lowering_error(
            node,
            "present/absent variants have the wrong closed fields",
        ));
    }
    Ok(())
}

fn literal_i64(node: &InlineNodeV1) -> Option<i64> {
    (node.evidence_type == "OrchValueV1" && node.config.0["operator"].as_str() == Some("literal"))
        .then(|| node.config.0["operand_config"]["value"].as_i64())
        .flatten()
}

fn map_lowering_error(node: &InlineNodeV1, message: impl Into<String>) -> OrchestrationError {
    invalid_slot(node.slot, "unsupported_lowering", message.into())
}

fn ports_with_role(node: &InlineNodeV1, role: OrchPortRoleV1) -> Vec<&InlinePortV1> {
    node.ports.iter().filter(|port| port.role == role).collect()
}

fn require_named_ports(
    node: &InlineNodeV1,
    role: OrchPortRoleV1,
    expected: &[&str],
) -> Result<(), OrchestrationError> {
    let actual = ports_with_role(node, role);
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected)
            .any(|(port, expected)| port.port != *expected)
    {
        return Err(invalid_slot(
            node.slot,
            "port_shape",
            format!("{role:?} ports must be exactly {expected:?}"),
        ));
    }
    Ok(())
}

fn reject_roles(node: &InlineNodeV1, roles: &[OrchPortRoleV1]) -> Result<(), OrchestrationError> {
    if node.ports.iter().any(|port| roles.contains(&port.role)) {
        Err(invalid_slot(
            node.slot,
            "port_shape",
            "node contains a port role excluded by its registry schema",
        ))
    } else {
        Ok(())
    }
}

fn validate_node_port_shape(
    node: &InlineNodeV1,
    nodes: &[InlineNodeV1],
) -> Result<(), OrchestrationError> {
    use OrchPortRoleV1::{Alternative, Config, Control, Data};
    match node.evidence_type.as_str() {
        "OrchValueV1" => {
            reject_roles(node, &[Alternative, Config])?;
            let config: ValueConfig = decode_config(node)?;
            let data = ports_with_role(node, Data);
            let expected = match config.operator {
                ValueOperator::Literal | ValueOperator::Input => Some(0),
                ValueOperator::Get => Some(1),
                ValueOperator::Index => Some(2),
                ValueOperator::Record => config
                    .operand_config
                    .0
                    .get("fields")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                ValueOperator::List => None,
                ValueOperator::Builtin => config
                    .operand_config
                    .0
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| match name {
                        "not" | "len" | "sha256" | "parse_json" => 1,
                        "slice" => 3,
                        _ => 2,
                    }),
            };
            if expected.is_some_and(|expected| data.len() != expected)
                || matches!(config.value_type, OrchTypeV1::List { max_items, .. } if data.len() > max_items as usize)
            {
                return Err(invalid_slot(
                    node.slot,
                    "value_arity",
                    "value operator data-port arity does not match its config",
                ));
            }
            Ok(())
        }
        "OrchArgumentV1" => {
            require_named_ports(node, Data, &["args"])?;
            if ports_with_role(node, Control).len() > 1 {
                return Err(invalid_slot(
                    node.slot,
                    "port_shape",
                    "argument accepts at most one control port",
                ));
            }
            reject_roles(node, &[Alternative, Config])
        }
        "OrchRetryPolicyV1" => {
            if node.ports.is_empty() {
                Ok(())
            } else {
                Err(invalid_slot(
                    node.slot,
                    "port_shape",
                    "retry policy cannot have parents",
                ))
            }
        }
        "OrchAskPauseV1" => {
            require_named_ports(node, Data, &["args"])?;
            if ports_with_role(node, Control).len() > 1 {
                return Err(invalid_slot(
                    node.slot,
                    "port_shape",
                    "ask accepts at most one prior-settlement control port",
                ));
            }
            reject_roles(node, &[Alternative, Config])
        }
        "OrchCallV1" => {
            require_named_ports(node, Data, &["args"])?;
            require_named_ports(node, Control, &["permit"])?;
            require_named_ports(node, Config, &["retry"])?;
            reject_roles(node, &[Alternative])
        }
        "OrchAwaitV1" => {
            require_named_ports(node, Data, &["operation"])?;
            reject_roles(node, &[Control, Alternative, Config])
        }
        "OrchBranchV1" => {
            require_named_ports(node, Data, &["subject"])?;
            if ports_with_role(node, Control).len() > 1 {
                return Err(invalid_slot(
                    node.slot,
                    "port_shape",
                    "branch accepts at most one control port",
                ));
            }
            reject_roles(node, &[Alternative, Config])
        }
        "OrchJoinV1" => {
            let mode = node.config.0["mode"].as_str().unwrap_or_default();
            reject_roles(node, &[Config])?;
            if mode == "select" {
                require_named_ports(node, Data, &["choice"])?;
                let alternatives = ports_with_role(node, Alternative);
                let choice = &nodes[node.ports[0].source_slot as usize];
                let cases = choice.config.0["cases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                if choice.evidence_type != "OrchBranchV1"
                    || alternatives.len() != cases.len()
                    || alternatives
                        .iter()
                        .zip(cases)
                        .any(|(port, case)| port.selector.as_deref() != Some(case))
                {
                    return Err(invalid_slot(
                        node.slot,
                        "select_join",
                        "select join alternatives must cover its branch cases in order",
                    ));
                }
            } else if !ports_with_role(node, Alternative).is_empty() {
                return Err(invalid_slot(
                    node.slot,
                    "all_join",
                    "all join cannot contain alternatives",
                ));
            }
            Ok(())
        }
        "OrchExitV1" => {
            require_named_ports(node, Data, &["value"])?;
            if ports_with_role(node, Control).len() > 1 {
                return Err(invalid_slot(
                    node.slot,
                    "port_shape",
                    "exit accepts at most one all-settled control port",
                ));
            }
            reject_roles(node, &[Alternative, Config])
        }
        _ => Ok(()),
    }
}

fn validate_producer_output(
    consumer: &InlineNodeV1,
    port: &InlinePortV1,
    producer: &InlineNodeV1,
) -> Result<(), OrchestrationError> {
    let allowed = match port.role {
        OrchPortRoleV1::Data => match producer.evidence_type.as_str() {
            "OrchValueV1" | "OrchArgumentV1" | "OrchAwaitV1" | "OrchJoinV1" => {
                port.output == "value"
            }
            "OrchCallV1" => port.output == "operation",
            "OrchBranchV1" => port.output == "choice",
            _ => false,
        },
        OrchPortRoleV1::Control => match producer.evidence_type.as_str() {
            "OrchValueV1" | "OrchArgumentV1" | "OrchAwaitV1" | "OrchJoinV1" | "OrchBranchV1" => {
                port.output == "settled"
            }
            "OrchAskPauseV1" => port.output == "permit",
            _ => false,
        },
        OrchPortRoleV1::Guard => {
            producer.evidence_type == "OrchBranchV1" && port.output == "choice"
        }
        OrchPortRoleV1::Alternative => {
            matches!(port.output.as_str(), "value" | "settled")
                && matches!(
                    producer.evidence_type.as_str(),
                    "OrchValueV1" | "OrchArgumentV1" | "OrchAwaitV1" | "OrchJoinV1"
                )
        }
        OrchPortRoleV1::Config => {
            producer.evidence_type == "OrchRetryPolicyV1" && port.output == "policy"
        }
        OrchPortRoleV1::Index => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(invalid_slot(
            consumer.slot,
            "producer_output",
            format!(
                "{}:{} cannot consume {}.{} as {:?}",
                consumer.evidence_type, port.port, producer.evidence_type, port.output, port.role
            ),
        ))
    }
}

fn node_region(node: &InlineNodeV1) -> Vec<RegionStepV1> {
    node.config
        .0
        .get("region")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn validate_region_guards(
    node: &InlineNodeV1,
    nodes: &[InlineNodeV1],
) -> Result<(), OrchestrationError> {
    let region = node_region(node);
    let guards = ports_with_role(node, OrchPortRoleV1::Guard);
    if guards.len() != region.len() {
        return Err(invalid_slot(
            node.slot,
            "region_guard",
            "region path and guard ports must have identical lengths",
        ));
    }
    for (guard, step) in guards.iter().zip(&region) {
        let producer = &nodes[guard.source_slot as usize];
        let cases = producer.config.0["cases"].as_array();
        if guard.source_slot != step.branch_slot
            || guard.selector.as_deref() != Some(step.case.as_str())
            || producer.evidence_type != "OrchBranchV1"
            || cases.is_none_or(|cases| {
                !cases
                    .iter()
                    .any(|case| case.as_str() == Some(step.case.as_str()))
            })
        {
            return Err(invalid_slot(
                node.slot,
                "region_guard",
                "guard does not prove the declared enclosing branch case",
            ));
        }
    }
    Ok(())
}

fn validate_region_flow(
    consumer: &InlineNodeV1,
    port: &InlinePortV1,
    producer: &InlineNodeV1,
) -> Result<(), OrchestrationError> {
    if matches!(port.role, OrchPortRoleV1::Guard | OrchPortRoleV1::Config) {
        return Ok(());
    }
    let consumer_region = node_region(consumer);
    let producer_region = node_region(producer);
    let ordinary = consumer_region.starts_with(&producer_region);
    let selected_escape = port.role == OrchPortRoleV1::Alternative
        && producer_region.len() == consumer_region.len() + 1
        && producer_region.starts_with(&consumer_region)
        && port.selector.as_deref() == producer_region.last().map(|step| step.case.as_str());
    if ordinary || selected_escape {
        Ok(())
    } else {
        Err(invalid_slot(
            consumer.slot,
            "region_leak",
            format!("port {} leaks a value across structured regions", port.port),
        ))
    }
}

fn validate_branch_coverage(nodes: &[InlineNodeV1]) -> Result<(), OrchestrationError> {
    for node in nodes
        .iter()
        .filter(|node| node.evidence_type == "OrchBranchV1")
    {
        let subject = port(node, OrchPortRoleV1::Data, "subject")
            .map(|port| &nodes[port.source_slot as usize]);
        let expected = subject.and_then(|producer| match producer.evidence_type.as_str() {
            "OrchValueV1" => serde_json::from_value::<ValueConfig>(producer.config.0.clone())
                .ok()
                .and_then(|config| match config.value_type {
                    OrchTypeV1::Bool => Some(vec!["true".to_owned(), "false".to_owned()]),
                    OrchTypeV1::Union { variants, .. } => {
                        Some(variants.into_keys().collect::<Vec<_>>())
                    }
                    _ => None,
                }),
            "OrchAwaitV1" => Some(if producer.config.0["on_error"].as_str() == Some("value") {
                vec!["completed".into(), "recoverable_error".into()]
            } else {
                vec!["completed".into()]
            }),
            _ => None,
        });
        let actual = node.config.0["cases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if expected.is_none_or(|expected| expected != actual) {
            return Err(invalid_slot(
                node.slot,
                "branch_coverage",
                "branch cases are not the exact exhaustive subject variants",
            ));
        }
    }
    Ok(())
}

fn regions_are_exclusive(left: &[RegionStepV1], right: &[RegionStepV1]) -> bool {
    left.iter()
        .zip(right)
        .any(|(left, right)| left.branch_slot == right.branch_slot && left.case != right.case)
}

fn validate_call_ordering(
    nodes: &[InlineNodeV1],
    read_groups: &[ReadGroupV1],
) -> Result<(), OrchestrationError> {
    let calls = nodes
        .iter()
        .filter(|node| node.evidence_type == "OrchCallV1")
        .collect::<Vec<_>>();
    for (index, earlier) in calls.iter().enumerate() {
        for later in calls.iter().skip(index + 1) {
            if read_groups.iter().any(|group| {
                group.members.contains(&earlier.slot) && group.members.contains(&later.slot)
            }) {
                continue;
            }
            if regions_are_exclusive(&node_region(earlier), &node_region(later)) {
                continue;
            }
            let earlier_await = nodes.iter().find(|node| {
                node.evidence_type == "OrchAwaitV1"
                    && port(node, OrchPortRoleV1::Data, "operation")
                        .is_some_and(|operation| operation.source_slot == earlier.slot)
            });
            let later_ask = port(later, OrchPortRoleV1::Control, "permit")
                .map(|permit| &nodes[permit.source_slot as usize]);
            let ordered = earlier_await.is_some_and(|settled| {
                later_ask.is_some_and(|ask| {
                    ports_with_role(ask, OrchPortRoleV1::Control)
                        .into_iter()
                        .any(|control| {
                            control.output == "settled"
                                && (control.source_slot == settled.slot
                                    || node_depends_on(control.source_slot, settled.slot, nodes))
                        })
                })
            });
            if !ordered {
                return Err(invalid_slot(
                    later.slot,
                    "unordered_calls",
                    format!(
                        "potentially co-active calls {} and {} require an Await-settled ordering edge or one read group",
                        earlier.slot, later.slot
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn validate_read_groups(
    nodes: &[InlineNodeV1],
    groups: &[ReadGroupV1],
) -> Result<(), OrchestrationError> {
    let mut ids = HashSet::new();
    let mut members = HashSet::new();
    for group in groups {
        validate_name(&group.id, "read group id")?;
        if !ids.insert(group.id.as_str())
            || group.members.is_empty()
            || group.max_concurrency == 0
            || group.max_concurrency > ORCHESTRATION_CONCURRENCY_MAX
        {
            return Err(invalid("read_group", "invalid or duplicate read group"));
        }
        let mut previous = None;
        let first = group.members[0];
        let mut awaits = Vec::new();
        for member in &group.members {
            if previous.is_some_and(|previous| previous >= *member) || !members.insert(*member) {
                return Err(invalid(
                    "read_group",
                    "group members must be unique and increasing",
                ));
            }
            let node = nodes
                .get(*member as usize)
                .ok_or_else(|| invalid("read_group", "group member does not exist"))?;
            if node.evidence_type != "OrchCallV1" {
                return Err(invalid_slot(
                    *member,
                    "read_group",
                    "only call definitions may be grouped",
                ));
            }
            if node.ports.iter().any(|port| port.source_slot >= first) {
                return Err(invalid_slot(
                    *member,
                    "read_group_preflight",
                    "every group member dependency must be ready before the first call slot",
                ));
            }
            let await_slot = nodes
                .iter()
                .find(|candidate| {
                    candidate.evidence_type == "OrchAwaitV1"
                        && port(candidate, OrchPortRoleV1::Data, "operation")
                            .is_some_and(|operation| operation.source_slot == *member)
                })
                .map(|node| node.slot)
                .ok_or_else(|| {
                    invalid_slot(*member, "read_group_barrier", "group call has no Await")
                })?;
            awaits.push(await_slot);
            previous = Some(*member);
        }
        let group_region = node_region(&nodes[first as usize]);
        let barrier = nodes.iter().find(|candidate| {
            candidate.evidence_type == "OrchJoinV1"
                && candidate.config.0["mode"].as_str() == Some("all")
                && node_region(candidate) == group_region
                && awaits.iter().all(|await_slot| {
                    candidate
                        .ports
                        .iter()
                        .any(|port| port.source_slot == *await_slot)
                })
        });
        let Some(barrier) = barrier else {
            return Err(invalid(
                "read_group_barrier",
                format!("read group {} has no all-join over every Await", group.id),
            ));
        };
        for exit in nodes.iter().filter(|candidate| {
            candidate.evidence_type == "OrchExitV1"
                && !regions_are_exclusive(&node_region(candidate), &group_region)
        }) {
            if !node_depends_on(exit.slot, barrier.slot, nodes) {
                return Err(invalid_slot(
                    exit.slot,
                    "read_group_barrier",
                    format!("exit does not depend on read group {} barrier", group.id),
                ));
            }
        }
    }
    Ok(())
}

fn node_depends_on(slot: u32, ancestor: u32, nodes: &[InlineNodeV1]) -> bool {
    let mut stack = vec![slot];
    let mut visited = HashSet::new();
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

/// Pinned canonical JSON encoding for pipe rows.  Object keys are sorted by
/// UTF-8 byte order, integer spelling is shortest decimal, and control
/// characters use lowercase `\u00xx` escapes.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, OrchestrationError> {
    let value = serde_json::to_value(value)
        .map_err(|error| invalid("canonical_json", error.to_string()))?;
    let mut output = Vec::new();
    write_canonical(&value, &mut output)?;
    Ok(output)
}

fn write_canonical(value: &Value, output: &mut Vec<u8>) -> Result<(), OrchestrationError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => {
            if !(number.is_i64() || number.is_u64()) {
                return Err(invalid(
                    "canonical_json",
                    "floating-point numbers are not permitted",
                ));
            }
            output.extend_from_slice(number.to_string().as_bytes());
        }
        Value::String(text) => write_string(text, output),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_string(key, output);
                output.push(b':');
                write_canonical(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn write_string(value: &str, output: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output.push(b'"');
    for character in value.chars() {
        match character {
            '"' => output.extend_from_slice(br#"\""#),
            '\\' => output.extend_from_slice(br#"\\"#),
            '\0'..='\u{1f}' => {
                let value = character as u8;
                output.extend_from_slice(b"\\u00");
                output.push(HEX[(value >> 4) as usize]);
                output.push(HEX[(value & 0x0f) as usize]);
            }
            _ => {
                let mut bytes = [0; 4];
                output.extend_from_slice(character.encode_utf8(&mut bytes).as_bytes());
            }
        }
    }
    output.push(b'"');
}

#[must_use]
pub fn request_digest(bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider/orchestration/request/v1\0");
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    format!("blake3:{}", hasher.finalize().to_hex())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn port(role: OrchPortRoleV1, port: &str, source_slot: u32, output: &str) -> InlinePortV1 {
        InlinePortV1 {
            role,
            port: port.into(),
            source_slot,
            output: output.into(),
            selector: None,
        }
    }

    fn one_call_graph() -> Vec<InlineNodeV1> {
        let digest = format!("blake3:{}", "0".repeat(64));
        vec![
            InlineNodeV1 {
                slot: 0,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator": "literal",
                    "type": {"kind":"record","fields":{}},
                    "operand_config": {"value": {}},
                    "region": []
                })),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 1,
                evidence_type: "OrchArgumentV1".into(),
                config: StrictJson(serde_json::json!({
                    "tool":"fs_read","wrapper_digest":digest,"region":[]
                })),
                ports: vec![port(OrchPortRoleV1::Data, "args", 0, "value")],
            },
            InlineNodeV1 {
                slot: 2,
                evidence_type: "OrchRetryPolicyV1".into(),
                config: StrictJson(serde_json::json!({"max_attempts":1})),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 3,
                evidence_type: "OrchAskPauseV1".into(),
                config: StrictJson(serde_json::json!({
                    "tool":"fs_read","wrapper_digest":digest,
                    "owner_call_slot":4,"wait_ms":120000,"region":[]
                })),
                ports: vec![port(OrchPortRoleV1::Data, "args", 1, "value")],
            },
            InlineNodeV1 {
                slot: 4,
                evidence_type: "OrchCallV1".into(),
                config: StrictJson(serde_json::json!({
                    "tool":"fs_read","wrapper_digest":digest,"region":[]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "args", 1, "value"),
                    port(OrchPortRoleV1::Control, "permit", 3, "permit"),
                    port(OrchPortRoleV1::Config, "retry", 2, "policy"),
                ],
            },
            InlineNodeV1 {
                slot: 5,
                evidence_type: "OrchAwaitV1".into(),
                config: StrictJson(serde_json::json!({"on_error":"stop","region":[]})),
                ports: vec![port(OrchPortRoleV1::Data, "operation", 4, "operation")],
            },
            InlineNodeV1 {
                slot: 6,
                evidence_type: "OrchExitV1".into(),
                config: StrictJson(serde_json::json!({"mode":"return","region":[]})),
                ports: vec![port(OrchPortRoleV1::Data, "value", 5, "value")],
            },
        ]
    }

    #[test]
    fn strict_json_rejects_nested_duplicates_and_floats() {
        let duplicate = br#"{"version":1,"transport":"instruct-pipe-dag-v1","catalog_digest":"blake3:0000000000000000000000000000000000000000000000000000000000000000","graph":{"kind":"inline","parameters":[],"nodes":[{"slot":0,"evidence_type":"OrchValueV1","config":{"a":1,"a":2},"ports":[]}],"exits":[0],"read_groups":[]},"inputs":[]}"#;
        assert_eq!(
            parse_script_request(duplicate)
                .expect_err("nested duplicate must reject")
                .code,
            "invalid_json"
        );
        let float =
            serde_json::from_str::<StrictJson>("[1.5]").expect_err("floating point must reject");
        assert!(float.to_string().contains("floating-point"));
    }

    #[test]
    fn canonical_json_pins_key_and_escape_order() {
        let value = serde_json::json!({"z": 2, "a": "\nλ", "list": [true, -7]});
        assert_eq!(
            canonical_json(&value).expect("canonical value"),
            "{\"a\":\"\\u000aλ\",\"list\":[true,-7],\"z\":2}".as_bytes()
        );
        assert!(canonical_json(&serde_json::json!(1.25)).is_err());
    }

    #[test]
    fn graph_rejects_non_dense_and_non_topological_slots() {
        let node = InlineNodeV1 {
            slot: 1,
            evidence_type: "OrchExitV1".into(),
            config: StrictJson(serde_json::json!({"mode":"return"})),
            ports: vec![],
        };
        assert_eq!(
            validate_inline_dag(&[], &[node], &[1], &[], None)
                .expect_err("non-dense graph must reject")
                .code,
            "non_dense_slot"
        );
    }

    #[test]
    fn one_call_family_has_exact_typed_ports_and_settlement() {
        let nodes = one_call_graph();
        let validated = validate_inline_dag(&[], &nodes, &[6], &[], None)
            .expect("one complete call family is valid");
        assert_eq!(validated.call_attempt_bound, 1);
        assert_eq!(validated.parent_entries, 7);
    }

    #[test]
    fn producer_outputs_and_branch_coverage_fail_closed() {
        let mut wrong_output = one_call_graph();
        wrong_output[6].ports[0].output = "operation".into();
        assert_eq!(
            validate_inline_dag(&[], &wrong_output, &[6], &[], None)
                .expect_err("Await.operation is not a declared output")
                .code,
            "producer_output"
        );

        let branch = vec![
            InlineNodeV1 {
                slot: 0,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":{"kind":"bool"},
                    "operand_config":{"value":true},"region":[]
                })),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 1,
                evidence_type: "OrchBranchV1".into(),
                config: StrictJson(serde_json::json!({"cases":["true"],"region":[]})),
                ports: vec![port(OrchPortRoleV1::Data, "subject", 0, "value")],
            },
            InlineNodeV1 {
                slot: 2,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":{"kind":"null"},
                    "operand_config":{"value":null},
                    "region":[{"branch_slot":1,"case":"true"}]
                })),
                ports: vec![InlinePortV1 {
                    role: OrchPortRoleV1::Guard,
                    port: "guard_true".into(),
                    source_slot: 1,
                    output: "choice".into(),
                    selector: Some("true".into()),
                }],
            },
            InlineNodeV1 {
                slot: 3,
                evidence_type: "OrchExitV1".into(),
                config: StrictJson(serde_json::json!({
                    "mode":"return","region":[{"branch_slot":1,"case":"true"}]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "value", 2, "value"),
                    InlinePortV1 {
                        role: OrchPortRoleV1::Guard,
                        port: "guard_true".into(),
                        source_slot: 1,
                        output: "choice".into(),
                        selector: Some("true".into()),
                    },
                ],
            },
        ];
        assert_eq!(
            validate_inline_dag(&[], &branch, &[3], &[], None)
                .expect_err("bool branch must cover both cases")
                .code,
            "branch_coverage"
        );
    }

    #[test]
    fn static_types_reject_invalid_builtin_in_unchosen_branch() {
        let guarded = |port_name: &str, case: &str| InlinePortV1 {
            role: OrchPortRoleV1::Guard,
            port: port_name.into(),
            source_slot: 1,
            output: "choice".into(),
            selector: Some(case.into()),
        };
        let nodes = vec![
            InlineNodeV1 {
                slot: 0,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":{"kind":"bool"},
                    "operand_config":{"value":true},"region":[]
                })),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 1,
                evidence_type: "OrchBranchV1".into(),
                config: StrictJson(serde_json::json!({"cases":["true","false"],"region":[]})),
                ports: vec![port(OrchPortRoleV1::Data, "subject", 0, "value")],
            },
            InlineNodeV1 {
                slot: 2,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":{"kind":"i64","min":0,"max":10},
                    "operand_config":{"value":1},
                    "region":[{"branch_slot":1,"case":"true"}]
                })),
                ports: vec![guarded("guard_true", "true")],
            },
            InlineNodeV1 {
                slot: 3,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":{"kind":"string","max_bytes":8},
                    "operand_config":{"value":"bad"},
                    "region":[{"branch_slot":1,"case":"false"}]
                })),
                ports: vec![guarded("guard_false", "false")],
            },
            InlineNodeV1 {
                slot: 4,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":{"kind":"i64","min":0,"max":10},
                    "operand_config":{"value":2},
                    "region":[{"branch_slot":1,"case":"false"}]
                })),
                ports: vec![guarded("guard_false", "false")],
            },
            InlineNodeV1 {
                slot: 5,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"builtin","type":{"kind":"i64","min":0,"max":20},
                    "operand_config":{"name":"add"},
                    "region":[{"branch_slot":1,"case":"false"}]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "left", 3, "value"),
                    port(OrchPortRoleV1::Data, "right", 4, "value"),
                    guarded("guard_false", "false"),
                ],
            },
            InlineNodeV1 {
                slot: 6,
                evidence_type: "OrchJoinV1".into(),
                config: StrictJson(serde_json::json!({
                    "mode":"select","type":{"kind":"i64","min":0,"max":20},
                    "omit_inactive":false,"region":[]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "choice", 1, "choice"),
                    InlinePortV1 {
                        role: OrchPortRoleV1::Alternative,
                        port: "true_value".into(),
                        source_slot: 2,
                        output: "value".into(),
                        selector: Some("true".into()),
                    },
                    InlinePortV1 {
                        role: OrchPortRoleV1::Alternative,
                        port: "false_value".into(),
                        source_slot: 5,
                        output: "value".into(),
                        selector: Some("false".into()),
                    },
                ],
            },
            InlineNodeV1 {
                slot: 7,
                evidence_type: "OrchExitV1".into(),
                config: StrictJson(serde_json::json!({"mode":"return","region":[]})),
                ports: vec![port(OrchPortRoleV1::Data, "value", 6, "value")],
            },
        ];
        let error = validate_inline_dag(&[], &nodes, &[7], &[], None)
            .expect_err("a type error in an unchosen arm must reject the whole graph");
        assert_eq!(error.code, "static_type");
        assert_eq!(error.slot, Some(5));
    }

    #[test]
    fn opaque_values_require_daemon_issued_provenance() {
        let opaque = OrchTypeV1::Opaque {
            ref_kind: "tool_result".into(),
            issuer_version: "1".into(),
            decoder_version: "1".into(),
        };
        let mut nodes = one_call_graph();
        assert_eq!(
            validate_inline_dag(&[opaque.clone()], &nodes, &[6], &[], None)
                .expect_err("raw request inputs cannot mint opaque capabilities")
                .code,
            "opaque_input_provenance"
        );

        nodes[0].config = StrictJson(serde_json::json!({
            "operator":"literal","type":opaque,
            "operand_config":{"value":{"fabricated":true}},"region":[]
        }));
        assert_eq!(
            validate_inline_dag(&[], &nodes, &[6], &[], None)
                .expect_err("literals cannot mint opaque capabilities")
                .code,
            "opaque_literal_provenance"
        );
    }

    #[test]
    fn omit_inactive_accepts_only_recognized_bounded_map_lowering() {
        let scalar = serde_json::json!({"kind":"i64","min":0,"max":10});
        let tag = serde_json::json!({"kind":"string","max_bytes":7});
        let presence = serde_json::json!({
            "kind":"union",
            "discriminant":"kind",
            "variants":{
                "absent":{"kind":"record","fields":{"kind":tag}},
                "present":{"kind":"record","fields":{"kind":tag,"value":scalar}}
            }
        });
        let guard = |case: &str| InlinePortV1 {
            role: OrchPortRoleV1::Guard,
            port: format!("guard_{case}"),
            source_slot: 4,
            output: "choice".into(),
            selector: Some(case.into()),
        };
        let nodes = vec![
            InlineNodeV1 {
                slot: 0,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal",
                    "type":{"kind":"list","item":scalar,"max_items":1},
                    "operand_config":{"value":[7]},"region":[]
                })),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 1,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":{"kind":"i64","min":0,"max":0},
                    "operand_config":{"value":0},"region":[]
                })),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 2,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"builtin","type":{"kind":"i64","min":0,"max":1},
                    "operand_config":{"name":"len"},"region":[]
                })),
                ports: vec![port(OrchPortRoleV1::Data, "value", 0, "value")],
            },
            InlineNodeV1 {
                slot: 3,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"builtin","type":{"kind":"bool"},
                    "operand_config":{"name":"lt"},"region":[]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "left", 1, "value"),
                    port(OrchPortRoleV1::Data, "right", 2, "value"),
                ],
            },
            InlineNodeV1 {
                slot: 4,
                evidence_type: "OrchBranchV1".into(),
                config: StrictJson(serde_json::json!({"cases":["true","false"],"region":[]})),
                ports: vec![port(OrchPortRoleV1::Data, "subject", 3, "value")],
            },
            InlineNodeV1 {
                slot: 5,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":tag,
                    "operand_config":{"value":"present"},
                    "region":[{"branch_slot":4,"case":"true"}]
                })),
                ports: vec![guard("true")],
            },
            InlineNodeV1 {
                slot: 6,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"index","type":scalar,"operand_config":{},
                    "region":[{"branch_slot":4,"case":"true"}]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "list", 0, "value"),
                    port(OrchPortRoleV1::Data, "index", 1, "value"),
                    guard("true"),
                ],
            },
            InlineNodeV1 {
                slot: 7,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"record","type":presence,
                    "operand_config":{"fields":["kind","value"]},
                    "region":[{"branch_slot":4,"case":"true"}]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "kind", 5, "value"),
                    port(OrchPortRoleV1::Data, "value", 6, "value"),
                    guard("true"),
                ],
            },
            InlineNodeV1 {
                slot: 8,
                evidence_type: "OrchValueV1".into(),
                config: StrictJson(serde_json::json!({
                    "operator":"literal","type":presence,
                    "operand_config":{"value":{"kind":"absent"}},
                    "region":[{"branch_slot":4,"case":"false"}]
                })),
                ports: vec![guard("false")],
            },
            InlineNodeV1 {
                slot: 9,
                evidence_type: "OrchJoinV1".into(),
                config: StrictJson(serde_json::json!({
                    "mode":"select","type":presence,"omit_inactive":false,"region":[]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "choice", 4, "choice"),
                    InlinePortV1 {
                        role: OrchPortRoleV1::Alternative,
                        port: "present".into(),
                        source_slot: 7,
                        output: "value".into(),
                        selector: Some("true".into()),
                    },
                    InlinePortV1 {
                        role: OrchPortRoleV1::Alternative,
                        port: "absent".into(),
                        source_slot: 8,
                        output: "value".into(),
                        selector: Some("false".into()),
                    },
                ],
            },
            InlineNodeV1 {
                slot: 10,
                evidence_type: "OrchJoinV1".into(),
                config: StrictJson(serde_json::json!({
                    "mode":"all",
                    "type":{"kind":"list","item":scalar,"max_items":1},
                    "omit_inactive":true,"region":[]
                })),
                ports: vec![port(OrchPortRoleV1::Data, "item_0", 9, "value")],
            },
            InlineNodeV1 {
                slot: 11,
                evidence_type: "OrchExitV1".into(),
                config: StrictJson(serde_json::json!({"mode":"return","region":[]})),
                ports: vec![port(OrchPortRoleV1::Data, "value", 10, "value")],
            },
        ];
        validate_inline_dag(&[], &nodes, &[11], &[], None)
            .expect("the exact bounded runtime-map lowering is valid");

        let mut forged = nodes;
        forged[1].config.0["operand_config"]["value"] = Value::from(1);
        forged[1].config.0["type"] = serde_json::json!({"kind":"i64","min":1,"max":1});
        assert_eq!(
            validate_inline_dag(&[], &forged, &[11], &[], None)
                .expect_err("a non-contiguous guard index cannot authorize omission")
                .code,
            "unsupported_lowering"
        );
    }

    #[test]
    fn two_coactive_calls_require_explicit_await_to_ask_ordering() {
        let mut nodes = one_call_graph();
        nodes.pop();
        let digest = format!("blake3:{}", "0".repeat(64));
        nodes.extend([
            InlineNodeV1 {
                slot: 6,
                evidence_type: "OrchArgumentV1".into(),
                config: StrictJson(serde_json::json!({
                    "tool":"fs_read","wrapper_digest":digest,"region":[]
                })),
                ports: vec![port(OrchPortRoleV1::Data, "args", 0, "value")],
            },
            InlineNodeV1 {
                slot: 7,
                evidence_type: "OrchRetryPolicyV1".into(),
                config: StrictJson(serde_json::json!({"max_attempts":1})),
                ports: vec![],
            },
            InlineNodeV1 {
                slot: 8,
                evidence_type: "OrchAskPauseV1".into(),
                config: StrictJson(serde_json::json!({
                    "tool":"fs_read","wrapper_digest":digest,
                    "owner_call_slot":9,"wait_ms":120000,"region":[]
                })),
                ports: vec![port(OrchPortRoleV1::Data, "args", 6, "value")],
            },
            InlineNodeV1 {
                slot: 9,
                evidence_type: "OrchCallV1".into(),
                config: StrictJson(serde_json::json!({
                    "tool":"fs_read","wrapper_digest":digest,"region":[]
                })),
                ports: vec![
                    port(OrchPortRoleV1::Data, "args", 6, "value"),
                    port(OrchPortRoleV1::Control, "permit", 8, "permit"),
                    port(OrchPortRoleV1::Config, "retry", 7, "policy"),
                ],
            },
            InlineNodeV1 {
                slot: 10,
                evidence_type: "OrchAwaitV1".into(),
                config: StrictJson(serde_json::json!({"on_error":"stop","region":[]})),
                ports: vec![port(OrchPortRoleV1::Data, "operation", 9, "operation")],
            },
            InlineNodeV1 {
                slot: 11,
                evidence_type: "OrchExitV1".into(),
                config: StrictJson(serde_json::json!({"mode":"return","region":[]})),
                ports: vec![
                    port(OrchPortRoleV1::Data, "value", 10, "value"),
                    port(OrchPortRoleV1::Control, "after_first", 5, "settled"),
                ],
            },
        ]);
        assert_eq!(
            validate_inline_dag(&[], &nodes, &[11], &[], None)
                .expect_err("data reachability is not effect ordering")
                .code,
            "unordered_calls"
        );
    }

    #[test]
    fn terminal_status_mapping_is_frozen() {
        assert_eq!(
            ScriptTerminalStatusV1::NeedsModel.outer_status(),
            ToolResultStatus::Completed
        );
        assert_eq!(
            ScriptTerminalStatusV1::Rejected.outer_status(),
            ToolResultStatus::Rejected
        );
        assert_eq!(
            ScriptTerminalStatusV1::TimedOut.outer_status(),
            ToolResultStatus::Failed
        );
        assert_eq!(
            ScriptTerminalStatusV1::OutcomeUnknown.outer_status(),
            ToolResultStatus::Unknown
        );
    }
}
