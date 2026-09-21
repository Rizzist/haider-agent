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
        validate_config(node, parameters.len(), &mut retry_attempts)?;
    }
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
            validate_named_types(variants, depth)
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
    parameter_count: usize,
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
            validate_value_operator(node, &config, parameter_count)?;
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
            if config.omit_inactive {
                return Err(invalid_slot(
                    node.slot,
                    "unsupported_lowering",
                    "omit_inactive requires validator-recognized map lowering",
                ));
            }
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
    parameter_count: usize,
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
            if object.len() != 1 || parameter as usize >= parameter_count {
                return Err(invalid_slot(
                    node.slot,
                    "operand_config",
                    "input parameter is not declared",
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
        *attempts = attempts
            .checked_add(u32::from(*retries.get(&retry.source_slot).unwrap_or(&1)))
            .ok_or_else(|| invalid("resource_overflow", "attempt count overflow"))?;
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
            previous = Some(*member);
        }
    }
    Ok(())
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
