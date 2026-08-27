//! Loom pipe/v1 — the typed-workflow DSL and agent-type vocabulary.
//!
//! The durable workflow format is **pipe source** (one terse line per node,
//! spec: `docs/design/loom-pipe-v1.md`). Loom authoring exposes an editable,
//! typed JSON document and validates it before lowering it to that retained
//! pipe source. [`parse_pipe`] turns the lowered source into an AST
//! **totally** — errors are collected, never thrown — and
//! [`compile_pipe`] lowers the AST onto the existing Convergence-Graph
//! vocabulary ([`GraphTemplateSpec`]/[`GraphNodeSpec`]/[`GraphGateKind`]) so
//! pinning, reduction, advancement and the TUI status surfaces execute Loom
//! workflows without new graph machinery. What is genuinely new — the
//! per-node agent type, task, and derived typed I/O — rides beside the
//! template as [`LoomNodeMeta`]. Red traversal is duplicated into the frozen
//! graph spec for runtime authority; the pipe source stays the structure of
//! record and the template remains a derived artifact.

use crate::graph::{
    GRAPH_MAX_ATTEMPTS, GRAPH_MAX_EVIDENCE_PER_ATTEMPT, GRAPH_TEMPLATE_VERSION, GraphExecutorShape,
    GraphGateKind, GraphNodeName, GraphNodeSpec, GraphTemplateSpec, graph_template,
    validate_graph_template,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// The DSL revision carried by every compiled workflow record.
pub const LOOM_PIPE_VERSION: &str = "pipe/v1";
/// Pipe source above this size is rejected before parsing.
pub const LOOM_SOURCE_MAX_BYTES: usize = 16 * 1024;
/// Editable JSON has structural overhead beyond the lowered pipe source.
pub const LOOM_AUTHOR_TEXT_MAX_BYTES: usize = 64 * 1024;
/// A node's quoted task line is display material; keep it terse.
pub const LOOM_TASK_MAX_BYTES: usize = 200;
/// Non-human nodes retry within the node this many times before settling red.
pub const LOOM_WORK_MAX_ATTEMPTS: u32 = 4;
/// Fan-out gates get a tighter in-node retry budget.
pub const LOOM_FANOUT_MAX_ATTEMPTS: u32 = 3;

/// One registered capability-scoped specialist. The registry (haider-store)
/// persists these; the compiler only reads the typed signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomAgentType {
    /// Registry key, referenced from pipe source as `@id`.
    pub id: String,
    pub name: String,
    /// The system prompt — the specialist's job.
    pub job: String,
    pub in_type: String,
    pub out_type: String,
    /// Capability grants: CLI names and API keys the type may touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clis: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<String>,
    /// Explicitly withheld capability keys (`cli:<program>` or
    /// `api:<host>`). Runtime grants remain the positive `clis`/`apis`
    /// lists; retaining denials makes the authored least-privilege decision
    /// part of the immutable content digest instead of discarding it at
    /// confirmation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denials: Vec<String>,
    /// Know-how: prose skills and frozen scripts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<String>,
    /// Display accent (hex) and glyph for the TUI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub glyph: String,
    pub rev: u32,
}

impl LoomAgentType {
    /// Stable CONTENT digest — the registry's rev counter is deliberately
    /// excluded, so re-registering identical content is detectable as a no-op.
    /// Display fields (color, glyph) participate: an accent-only edit is a
    /// real revision the registry must persist, never a silent no-op.
    #[must_use]
    pub fn digest(&self) -> String {
        // Length-prefixed framing (review round 3): delimiter bytes inside a
        // field can never collide with field boundaries, so `["a","b"]` and
        // `["a\u{1e}b"]` hash differently and a revocation never no-ops.
        let mut hasher = blake3::Hasher::new();
        let mut part = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        for field in [
            self.id.as_str(),
            self.name.as_str(),
            self.job.as_str(),
            self.in_type.as_str(),
            self.out_type.as_str(),
            self.color.as_str(),
            self.glyph.as_str(),
        ] {
            part(field.as_bytes());
        }
        for list in [&self.clis, &self.apis, &self.skills, &self.scripts] {
            part(&(list.len() as u64).to_le_bytes());
            for item in list {
                part(item.as_bytes());
            }
        }
        // Preserve every pre-authoring digest byte-for-byte when no explicit
        // denial was authored. New denials are nevertheless content-bearing.
        if !self.denials.is_empty() {
            part(&(self.denials.len() as u64).to_le_bytes());
            for item in &self.denials {
                part(item.as_bytes());
            }
        }
        hasher.finalize().to_hex()[..32].to_string()
    }

    /// The compiler's view of this type.
    #[must_use]
    pub fn signature(&self) -> LoomTypeSig {
        LoomTypeSig {
            in_type: self.in_type.clone(),
            out_type: self.out_type.clone(),
        }
    }
}

/// A work node's typed signature, resolved from the registry at compile time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoomTypeSig {
    pub in_type: String,
    pub out_type: String,
}

/// Source-level gate vocabulary. `Ship` compiles to `CommandGreen` for now —
/// a reviewer child is still a child whose gate command exits green; a
/// dedicated reviewer gate can widen the mapping without touching sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoomGate {
    Cmd,
    Ship,
    AllOf(u32),
    Human,
}

/// One parsed node line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomNodeAst {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    pub gate: LoomGate,
    /// Explicit incoming green dependencies. `None` preserves pipe/v1's
    /// implicit dependency on the previous source line; `Some` is authored as
    /// one compact `<-first,second` clause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
    /// Conditional red target. The node's own name represents an authored
    /// self-loop (`↻`); any other value is an earlier ancestor (`↺`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub back: Option<String>,
}

/// A totally-parsed pipe. `errors` non-empty means the source is rejected at
/// registration; the AST still carries everything that did parse so the
/// author sees the whole picture, not the first failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomAst {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub in_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub out_type: String,
    pub nodes: Vec<LoomNodeAst>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Loom-specific compiled facts for one node, carried beside the CG template
/// (which has no vocabulary for agent types, tasks, or typed I/O).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomNodeMeta {
    /// The source-level (lowercase) name.
    pub source_name: String,
    /// The CG node identity (uppercased source name).
    pub node: GraphNodeName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Exact registry contract resolved when this workflow revision compiled.
    /// Store-backed compilation fills both fields for typed nodes; absence is
    /// a legacy/unbound contract and runtime dispatch fails closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type_rev: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type_digest: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub back: Option<GraphNodeName>,
}

/// One compiled, registrable workflow: pipe source of record + the derived CG
/// template + the Loom node metadata + a stable digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomWorkflow {
    pub id: String,
    pub pipe_version: String,
    pub source: String,
    pub in_type: String,
    pub out_type: String,
    pub template: GraphTemplateSpec,
    pub meta: Vec<LoomNodeMeta>,
    pub rev: u32,
    pub digest: String,
}

impl LoomWorkflow {
    /// Recompute content identity after the store binds exact agent contracts
    /// to compiler metadata. Pure compiler callers leave those optional facts
    /// empty and retain their deterministic source/signature digest.
    pub fn refresh_digest(&mut self) {
        self.digest = workflow_digest(self);
    }
}

/// Registry outcome for one register call (agent type or workflow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomRegistration {
    pub id: String,
    pub rev: u32,
    pub digest: String,
    /// False when the call was an idempotent same-content no-op.
    pub updated: bool,
}

/// Registry namespace named by CAS, archive, validation, and watch surfaces.
/// Agent-type lineage and workflow DAGs remain distinct records; this enum is
/// only their shared registry coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoomRegistryEntryKind {
    AgentType,
    Workflow,
    #[serde(other)]
    Unknown,
}

/// One client-observed registry head used as a compare-and-swap fence.
/// Revision zero explicitly means "this id must not exist". Digest absence is
/// a typed weaker fence, not permission to invent a digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomRevisionExpectation {
    pub rev: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// Typed registry CAS loss. Current coordinates are optional because an
/// expected existing row may have become absent; no sentinel revision or
/// fabricated digest stands in for that absence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomRevisionConflict {
    pub expected: LoomRevisionExpectation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_rev: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_digest: Option<String>,
}

/// Exact current registry address and archive selection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomRegistryEntryRef {
    pub kind: LoomRegistryEntryKind,
    pub id: String,
    pub rev: u32,
    pub digest: String,
    pub archived: bool,
}

/// Full typed record carried by a baseline or delta. The tag prevents agent
/// lineage records from being collapsed into workflow graph records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoomRegistryRecord {
    AgentType(LoomAgentType),
    Workflow(LoomWorkflow),
    Unknown,
}

impl<'de> Deserialize<'de> for LoomRegistryRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // `#[serde(other)]` on the adjacent representation above treats the
        // catch-all as a unit variant and then rejects an unknown variant's
        // `record` map. Decode the same wire shape internally tagged instead:
        // known records stay typed, while an unknown tag consumes and ignores
        // any future payload without making it actionable.
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum RegistryRecordWire {
            AgentType {
                record: LoomAgentType,
            },
            Workflow {
                record: LoomWorkflow,
            },
            #[serde(other)]
            Unknown,
        }

        Ok(match RegistryRecordWire::deserialize(deserializer)? {
            RegistryRecordWire::AgentType { record } => Self::AgentType(record),
            RegistryRecordWire::Workflow { record } => Self::Workflow(record),
            RegistryRecordWire::Unknown => Self::Unknown,
        })
    }
}

impl LoomRegistryRecord {
    #[must_use]
    pub const fn kind(&self) -> LoomRegistryEntryKind {
        match self {
            Self::AgentType(_) => LoomRegistryEntryKind::AgentType,
            Self::Workflow(_) => LoomRegistryEntryKind::Workflow,
            Self::Unknown => LoomRegistryEntryKind::Unknown,
        }
    }
}

/// Durable registry change vocabulary. A content mutation emits `upserted`
/// and, when it minted an immutable revision, `revision_added`; archive state
/// transitions remain separate facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoomRegistryDeltaKind {
    Upserted,
    Archived,
    Unarchived,
    RevisionAdded,
    #[serde(other)]
    Unknown,
}

/// One replayable, persist-before-publish registry event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomRegistryDelta {
    pub cursor: u64,
    pub change: LoomRegistryDeltaKind,
    pub entry: LoomRegistryEntryRef,
    pub record: LoomRegistryRecord,
}

/// One full registry baseline sealed at `through_cursor`. Archived records are
/// included so a baseline can repair any prior gap without client polling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomRegistrySnapshot {
    pub through_cursor: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<LoomRegistrySnapshotEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomRegistrySnapshotEntry {
    /// Exact daemon-owned coordinate. In particular, agent-type digests are
    /// carried rather than left for clients to recompute from record fields.
    pub entry: LoomRegistryEntryRef,
    pub record: LoomRegistryRecord,
}

/// The two typed documents accepted by the Loom authoring RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoomAuthorKind {
    AgentType,
    Workflow,
}

/// Stable classification for an authoring validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoomAuthorValidationCode {
    Syntax,
    InvalidField,
    MissingField,
    DuplicateValue,
    CapabilityContradiction,
    UnknownAgentType,
    TypeMismatch,
    InvalidGraph,
}

/// Source coordinates into the exact editable text returned by the RPC.
/// Lines and columns are one-based; `field` is a stable dotted/indexed path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomAuthorLocation {
    pub line: u32,
    pub column: u32,
    pub field: String,
}

/// One typed, location-bearing authoring rejection. Callers branch on
/// `code`/`location.field`; `message` is display prose only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomAuthorValidationError {
    pub code: LoomAuthorValidationCode,
    pub message: String,
    pub location: LoomAuthorLocation,
}

/// One editable authoring revision. A successful validation has no errors;
/// the text remains editable and is not registry authority until confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomAuthorDraft {
    pub authoring_id: String,
    /// Monotonic daemon-owned edit fence for this authoring session.
    pub revision: u64,
    pub kind: LoomAuthorKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<LoomAuthorValidationError>,
}

/// Confirmation receipt. `execution_digest` is daemon-issued: a typed-agent
/// content digest or the workflow template digest accepted by the existing
/// `workflow_instance_v1` graph fence. Clients never compute either value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomAuthorConfirmed {
    pub authoring_id: String,
    pub kind: LoomAuthorKind,
    pub canonical_text: String,
    pub registration: LoomRegistration,
    pub execution_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_job_id: Option<String>,
}

/// A capability decision in authoring text. Every `capability_keys` entry is
/// required to occur exactly once across `grants` and `denials`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoomAuthorAgentTypeSpec {
    pub id: String,
    pub name: String,
    pub job: String,
    pub in_type: String,
    pub out_type: String,
    #[serde(default)]
    pub capability_keys: Vec<String>,
    #[serde(default)]
    pub grants: Vec<String>,
    #[serde(default)]
    pub denials: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub scripts: Vec<String>,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub glyph: String,
}

/// The only evidence frame contract authored in v1. The fixed protocol/tool
/// names make it a typed statement, while `required_green` lowers exactly to
/// the existing command/all-of gate and therefore participates in the
/// executable workflow digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoomAuthorEvidenceContract {
    pub protocol: String,
    pub tool: String,
    pub required_green: u32,
}

/// One typed authoring node. Edges are explicit so forks, joins, and back
/// edges remain visible/editable instead of being inferred from prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoomAuthorNodeSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub task: String,
    pub in_type: String,
    pub out_type: String,
    /// `command`, `review`, `human`, or `all_of`.
    pub gate: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub back_edge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<LoomAuthorEvidenceContract>,
}

/// Editable workflow document. Node order is deterministic topological order
/// and is preserved in canonical text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoomAuthorWorkflowSpec {
    pub id: String,
    pub in_type: String,
    pub out_type: String,
    pub nodes: Vec<LoomAuthorNodeSpec>,
}

/// The tagged text format used by the editor and all authoring RPCs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoomAuthorSpec {
    AgentType(LoomAuthorAgentTypeSpec),
    Workflow(LoomAuthorWorkflowSpec),
}

/// Validated lowering consumed by daemon confirmation. It is deliberately
/// not serializable: only the daemon may turn editable text into a registry
/// mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedLoomAuthorSpec {
    AgentType {
        record: Box<LoomAgentType>,
        canonical_text: String,
    },
    Workflow {
        source: String,
        canonical_text: String,
    },
}

impl ValidatedLoomAuthorSpec {
    #[must_use]
    pub const fn kind(&self) -> LoomAuthorKind {
        match self {
            Self::AgentType { .. } => LoomAuthorKind::AgentType,
            Self::Workflow { .. } => LoomAuthorKind::Workflow,
        }
    }
}

/// Parse, normalize, and validate one editable authoring revision. Validation
/// is total and returns every semantic problem it can identify with a source
/// coordinate. The registry lookup is read-only and supplies exact typed node
/// signatures; confirmation repeats this validation against the current
/// registry immediately before mutation.
pub fn validate_loom_author_text(
    text: &str,
    expected_kind: LoomAuthorKind,
    lookup: impl Fn(&str) -> Option<LoomTypeSig>,
) -> Result<ValidatedLoomAuthorSpec, Vec<LoomAuthorValidationError>> {
    if text.len() > LOOM_AUTHOR_TEXT_MAX_BYTES {
        return Err(vec![author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            format!("authoring text exceeds {LOOM_AUTHOR_TEXT_MAX_BYTES} bytes"),
            "$",
            None,
        )]);
    }
    #[derive(Deserialize)]
    struct KindEnvelope {
        kind: LoomAuthorKind,
    }

    // Deserialize the tag without buffering the tagged enum's payload.
    // serde's internally-tagged enum path loses nested data coordinates
    // (`line = column = 0`) after buffering, which collapses an edited leaf
    // such as `nodes[0].evidence.required_green` back to `$`.
    let actual_kind = serde_json::from_str::<KindEnvelope>(text)
        .map_err(|error| loom_author_decode_errors(text, error))?
        .kind;
    if actual_kind != expected_kind {
        return Err(vec![author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "authoring kind does not match the session",
            "kind",
            None,
        )]);
    }
    // Decode the known payload as its strict struct directly. Blanking the
    // top-level tag keeps every byte coordinate stable while avoiding the
    // internally-tagged enum's coordinate-erasing buffer.
    let payload = json_without_top_level_field(text, "kind").ok_or_else(|| {
        vec![author_error(
            text,
            LoomAuthorValidationCode::MissingField,
            "authoring text is missing its kind tag",
            "kind",
            None,
        )]
    })?;
    match actual_kind {
        LoomAuthorKind::AgentType => {
            let spec = serde_json::from_str::<LoomAuthorAgentTypeSpec>(&payload)
                .map_err(|error| loom_author_decode_errors(text, error))?;
            validate_author_agent_type(text, spec)
        }
        LoomAuthorKind::Workflow => {
            let spec = serde_json::from_str::<LoomAuthorWorkflowSpec>(&payload)
                .map_err(|error| loom_author_decode_errors(text, error))?;
            validate_author_workflow(text, spec, lookup)
        }
    }
}

fn loom_author_decode_errors(
    text: &str,
    error: serde_json::Error,
) -> Vec<LoomAuthorValidationError> {
    let message = error.to_string();
    let missing = json_error_field(&message, "missing field");
    let duplicate = json_error_field(&message, "duplicate field");
    let unknown = json_error_field(&message, "unknown field");
    let code = if missing.is_some() {
        LoomAuthorValidationCode::MissingField
    } else if duplicate.is_some() {
        LoomAuthorValidationCode::DuplicateValue
    } else if matches!(
        error.classify(),
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof
    ) {
        LoomAuthorValidationCode::Syntax
    } else {
        LoomAuthorValidationCode::InvalidField
    };
    let leaf = missing.or(duplicate).or(unknown);
    let field = if code == LoomAuthorValidationCode::Syntax {
        "$".to_owned()
    } else {
        json_error_path(text, error.line(), error.column(), code, leaf)
    };
    vec![LoomAuthorValidationError {
        code,
        message,
        location: LoomAuthorLocation {
            line: u32::try_from(error.line()).unwrap_or(u32::MAX).max(1),
            column: u32::try_from(error.column()).unwrap_or(u32::MAX).max(1),
            field,
        },
    }]
}

fn json_error_field<'a>(message: &'a str, class: &str) -> Option<&'a str> {
    let suffix = message.split_once(class)?.1;
    let (_, suffix) = suffix.split_once('`')?;
    suffix.split_once('`').map(|(field, _)| field)
}

#[derive(Debug)]
struct JsonPathSpan {
    start: usize,
    end: usize,
    path: String,
    key: Option<String>,
    container: bool,
}

struct JsonPathScanner<'a> {
    text: &'a str,
    bytes: &'a [u8],
    cursor: usize,
    spans: Vec<JsonPathSpan>,
}

impl<'a> JsonPathScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            cursor: 0,
            spans: Vec::new(),
        }
    }

    fn scan(mut self) -> Vec<JsonPathSpan> {
        let _ = self.value("$");
        self.spans
    }

    fn value(&mut self, path: &str) -> Option<()> {
        self.whitespace();
        let start = self.cursor;
        match self.bytes.get(self.cursor).copied()? {
            b'{' => self.object(path, start),
            b'[' => self.array(path, start),
            b'"' => {
                self.string()?;
                self.spans.push(JsonPathSpan {
                    start,
                    end: self.cursor,
                    path: path.to_owned(),
                    key: None,
                    container: false,
                });
                Some(())
            }
            _ => {
                while self.cursor < self.bytes.len()
                    && !matches!(
                        self.bytes[self.cursor],
                        b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}'
                    )
                {
                    self.cursor += 1;
                }
                self.spans.push(JsonPathSpan {
                    start,
                    end: self.cursor,
                    path: path.to_owned(),
                    key: None,
                    container: false,
                });
                Some(())
            }
        }
    }

    fn object(&mut self, path: &str, start: usize) -> Option<()> {
        self.cursor += 1;
        self.whitespace();
        while self.bytes.get(self.cursor).copied()? != b'}' {
            let key_start = self.cursor;
            let key = self.string()?;
            let key_end = self.cursor;
            let child_path = json_child_path(path, &key);
            self.spans.push(JsonPathSpan {
                start: key_start,
                end: key_end,
                path: child_path.clone(),
                key: Some(key),
                container: false,
            });
            self.whitespace();
            if self.bytes.get(self.cursor).copied()? != b':' {
                return None;
            }
            self.cursor += 1;
            self.value(&child_path)?;
            self.whitespace();
            match self.bytes.get(self.cursor).copied()? {
                b',' => {
                    self.cursor += 1;
                    self.whitespace();
                }
                b'}' => break,
                _ => return None,
            }
        }
        self.cursor += 1;
        self.spans.push(JsonPathSpan {
            start,
            end: self.cursor,
            path: path.to_owned(),
            key: None,
            container: true,
        });
        Some(())
    }

    fn array(&mut self, path: &str, start: usize) -> Option<()> {
        self.cursor += 1;
        self.whitespace();
        let mut index = 0;
        while self.bytes.get(self.cursor).copied()? != b']' {
            let child_path = format!("{path}[{index}]");
            self.value(&child_path)?;
            index += 1;
            self.whitespace();
            match self.bytes.get(self.cursor).copied()? {
                b',' => {
                    self.cursor += 1;
                    self.whitespace();
                }
                b']' => break,
                _ => return None,
            }
        }
        self.cursor += 1;
        self.spans.push(JsonPathSpan {
            start,
            end: self.cursor,
            path: path.to_owned(),
            key: None,
            container: true,
        });
        Some(())
    }

    fn string(&mut self) -> Option<String> {
        let start = self.cursor;
        if self.bytes.get(self.cursor).copied()? != b'"' {
            return None;
        }
        self.cursor += 1;
        while let Some(byte) = self.bytes.get(self.cursor).copied() {
            match byte {
                b'"' => {
                    self.cursor += 1;
                    return serde_json::from_str(&self.text[start..self.cursor]).ok();
                }
                b'\\' => {
                    self.cursor += 2;
                }
                _ => self.cursor += 1,
            }
        }
        None
    }

    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.cursor += 1;
        }
    }
}

fn json_child_path(parent: &str, child: &str) -> String {
    if parent == "$" {
        child.to_owned()
    } else {
        format!("{parent}.{child}")
    }
}

fn json_error_offset(text: &str, line: usize, column: usize) -> usize {
    let line_start = text
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum::<usize>();
    // serde_json reports a byte column, not a Unicode scalar column.
    let line_text = text.get(line_start..).unwrap_or_default();
    let line_bytes = line_text.find('\n').unwrap_or(line_text.len());
    let column_offset = column.saturating_sub(1).min(line_bytes);
    line_start.saturating_add(column_offset).min(text.len())
}

fn json_error_path(
    text: &str,
    line: usize,
    column: usize,
    code: LoomAuthorValidationCode,
    leaf: Option<&str>,
) -> String {
    let offset = json_error_offset(text, line, column);
    let spans = JsonPathScanner::new(text).scan();
    if matches!(
        code,
        LoomAuthorValidationCode::DuplicateValue | LoomAuthorValidationCode::InvalidField
    ) && let Some(leaf) = leaf
        && let Some(span) = spans
            .iter()
            .filter(|span| span.key.as_deref() == Some(leaf) && span.start <= offset)
            .min_by_key(|span| offset.saturating_sub(span.start))
    {
        return span.path.clone();
    }
    let probe = offset.saturating_sub(1);
    let enclosing = spans
        .iter()
        .filter(|span| span.start <= offset && (offset <= span.end || probe <= span.end))
        .max_by_key(|span| span.path.len());
    if code == LoomAuthorValidationCode::MissingField {
        let parent = enclosing
            .filter(|span| span.container)
            .or_else(|| {
                spans
                    .iter()
                    .filter(|span| span.container && span.start <= offset && probe <= span.end)
                    .max_by_key(|span| span.path.len())
            })
            .map_or("$", |span| span.path.as_str());
        return leaf.map_or_else(|| parent.to_owned(), |field| json_child_path(parent, field));
    }
    enclosing.map_or_else(|| leaf.unwrap_or("$").to_owned(), |span| span.path.clone())
}

fn json_without_top_level_field(text: &str, field: &str) -> Option<String> {
    let spans = JsonPathScanner::new(text).scan();
    let key = spans
        .iter()
        .find(|span| span.path == field && span.key.as_deref() == Some(field))?;
    let value = spans
        .iter()
        .filter(|span| span.path == field && span.key.is_none() && span.start >= key.end)
        .min_by_key(|span| span.start)?;
    let source = text.as_bytes();
    let mut start = key.start;
    let mut end = value.end;
    let mut after = end;
    while source.get(after).is_some_and(u8::is_ascii_whitespace) {
        after += 1;
    }
    if source.get(after) == Some(&b',') {
        end = after + 1;
    } else {
        let mut before = start;
        while before > 0 && source[before - 1].is_ascii_whitespace() {
            before -= 1;
        }
        if before > 0 && source[before - 1] == b',' {
            start = before - 1;
        }
    }
    let mut payload = source.to_vec();
    for byte in &mut payload[start..end] {
        if !matches!(*byte, b'\n' | b'\r') {
            *byte = b' ';
        }
    }
    String::from_utf8(payload).ok()
}

fn validate_author_agent_type(
    text: &str,
    mut spec: LoomAuthorAgentTypeSpec,
) -> Result<ValidatedLoomAuthorSpec, Vec<LoomAuthorValidationError>> {
    spec.in_type = spec.in_type.trim().to_owned();
    spec.out_type = spec.out_type.trim().to_owned();
    for key in spec
        .capability_keys
        .iter_mut()
        .chain(spec.grants.iter_mut())
        .chain(spec.denials.iter_mut())
    {
        *key = normalize_capability_key(key);
    }
    let mut errors = Vec::new();
    if !is_ident(&spec.id) {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "agent type id must be a 1..=64 byte identifier",
            "id",
            None,
        ));
    }
    if spec.name.trim().is_empty()
        || spec.name.len() > 120
        || spec.name.chars().any(char::is_control)
    {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "agent type name must be 1..=120 bytes of safe text",
            "name",
            None,
        ));
    }
    if spec.job.trim().is_empty()
        || spec.job.len() > 4 * 1024
        || spec
            .job
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "agent type job must be 1..=4096 bytes of safe text",
            "job",
            None,
        ));
    }
    for (field, type_expr) in [("in_type", &spec.in_type), ("out_type", &spec.out_type)] {
        if !valid_type_expr(type_expr) {
            errors.push(author_error(
                text,
                LoomAuthorValidationCode::InvalidField,
                "type must be a bounded identifier or `A + B` expression",
                field,
                None,
            ));
        }
    }
    for (field, values) in [
        ("capability_keys", &spec.capability_keys),
        ("grants", &spec.grants),
        ("denials", &spec.denials),
    ] {
        validate_capability_list(text, field, values, &mut errors);
    }
    let keys = spec.capability_keys.iter().collect::<HashSet<_>>();
    let grants = spec.grants.iter().collect::<HashSet<_>>();
    let denials = spec.denials.iter().collect::<HashSet<_>>();
    let dispositions = grants.union(&denials).copied().collect::<HashSet<_>>();
    for key in grants.intersection(&denials) {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::CapabilityContradiction,
            format!("capability `{key}` is both granted and denied"),
            "denials",
            Some(key),
        ));
    }
    for key in keys.difference(&dispositions) {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::MissingField,
            format!("capability `{key}` has no grant or denial"),
            "capability_keys",
            Some(key),
        ));
    }
    for key in grants.union(&denials) {
        if !keys.contains(key) {
            errors.push(author_error(
                text,
                LoomAuthorValidationCode::CapabilityContradiction,
                format!("capability disposition `{key}` is not declared in capability_keys"),
                if grants.contains(key) {
                    "grants"
                } else {
                    "denials"
                },
                Some(key),
            ));
        }
    }
    for grant in &spec.grants {
        let valid = grant.strip_prefix("cli:").map_or_else(
            || {
                grant
                    .strip_prefix("api:")
                    .is_some_and(valid_author_api_host)
            },
            valid_author_cli_grant,
        );
        if !valid {
            errors.push(author_error(
                text,
                LoomAuthorValidationCode::InvalidField,
                format!("capability `{grant}` cannot be granted by the typed-agent fence"),
                "grants",
                Some(grant),
            ));
        }
    }
    for (field, values) in [("skills", &spec.skills), ("scripts", &spec.scripts)] {
        validate_bounded_list(text, field, values, &mut errors);
    }
    let color_ok = spec.color.is_empty()
        || (spec.color.len() == 7
            && spec.color.starts_with('#')
            && spec
                .color
                .bytes()
                .skip(1)
                .all(|byte| byte.is_ascii_hexdigit()));
    if !color_ok {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "color must be empty or `#rrggbb`",
            "color",
            None,
        ));
    }
    let leads_combining = spec.glyph.chars().next().is_some_and(|character| {
        matches!(
            character,
            '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{20D0}'..='\u{20FF}'
        )
    });
    if spec.glyph.len() > 16
        || leads_combining
        || spec.glyph.chars().any(author_invisible_character)
    {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "glyph must be at most 16 bytes, lead with a base, and contain no invisible characters",
            "glyph",
            None,
        ));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    // Capability decisions are sets. Canonical authoring text and its
    // lowered digest must not move merely because a user reordered JSON
    // entries while editing.
    spec.capability_keys.sort();
    spec.grants.sort();
    spec.denials.sort();
    let mut clis = Vec::new();
    let mut apis = Vec::new();
    for grant in &spec.grants {
        if let Some(value) = grant.strip_prefix("cli:") {
            clis.push(value.to_owned());
        } else if let Some(value) = grant.strip_prefix("api:") {
            apis.push(value.to_owned());
        }
    }
    let canonical_text = serde_json::to_string_pretty(&LoomAuthorSpec::AgentType(spec.clone()))
        .map_err(|error| {
            vec![author_error(
                text,
                LoomAuthorValidationCode::InvalidField,
                format!("cannot canonicalize agent type: {error}"),
                "$",
                None,
            )]
        })?;
    if canonical_text.len() > LOOM_AUTHOR_TEXT_MAX_BYTES {
        return Err(vec![author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            format!("canonical authoring text exceeds {LOOM_AUTHOR_TEXT_MAX_BYTES} bytes"),
            "$",
            None,
        )]);
    }
    Ok(ValidatedLoomAuthorSpec::AgentType {
        record: Box::new(LoomAgentType {
            id: spec.id,
            name: spec.name,
            job: spec.job,
            in_type: spec.in_type,
            out_type: spec.out_type,
            clis,
            apis,
            denials: spec.denials,
            skills: spec.skills,
            scripts: spec.scripts,
            color: spec.color,
            glyph: spec.glyph,
            rev: 0,
        }),
        canonical_text,
    })
}

fn validate_author_workflow(
    text: &str,
    mut spec: LoomAuthorWorkflowSpec,
    lookup: impl Fn(&str) -> Option<LoomTypeSig>,
) -> Result<ValidatedLoomAuthorSpec, Vec<LoomAuthorValidationError>> {
    spec.in_type = spec.in_type.trim().to_owned();
    spec.out_type = spec.out_type.trim().to_owned();
    for node in &mut spec.nodes {
        node.in_type = node.in_type.trim().to_owned();
        node.out_type = node.out_type.trim().to_owned();
    }
    let mut errors = Vec::new();
    if !is_ident(&spec.id) {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "workflow id must be a 1..=64 byte identifier",
            "id",
            None,
        ));
    }
    if graph_template(&spec.id).is_some() {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidGraph,
            "workflow id is reserved by a built-in graph template",
            "id",
            Some(&spec.id),
        ));
    }
    for (field, type_expr) in [("in_type", &spec.in_type), ("out_type", &spec.out_type)] {
        if !valid_type_expr(type_expr) {
            errors.push(author_error(
                text,
                LoomAuthorValidationCode::InvalidField,
                "type must be a bounded identifier or `A + B` expression",
                field,
                None,
            ));
        }
    }
    if spec.nodes.is_empty() {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::MissingField,
            "workflow must declare at least one node",
            "nodes",
            None,
        ));
    }
    let mut seen = HashSet::new();
    let mut seen_graph_nodes = HashSet::new();
    let mut authored_outputs = HashMap::<&str, String>::new();
    let mut authored_merged_outputs = HashSet::<&str>::new();
    let mut source = format!("{}: {} -> {}", spec.id, spec.in_type, spec.out_type);
    for (index, node) in spec.nodes.iter().enumerate() {
        let base = format!("nodes[{index}]");
        let graph_node_id = node.id.to_ascii_uppercase();
        if GraphNodeName::new(graph_node_id.clone()).is_err() {
            errors.push(author_node_error(
                text,
                &spec,
                index,
                LoomAuthorValidationCode::InvalidField,
                "node id must begin with a letter and contain only letters, digits, `_`, or `-`",
                "id",
            ));
        } else if !seen.insert(node.id.as_str()) || !seen_graph_nodes.insert(graph_node_id) {
            errors.push(author_node_error(
                text,
                &spec,
                index,
                LoomAuthorValidationCode::DuplicateValue,
                format!("node `{}` is duplicated", node.id),
                "id",
            ));
        }
        if node.task.len() > LOOM_TASK_MAX_BYTES
            || node
                .task
                .chars()
                .any(|character| character == '"' || character.is_control())
        {
            errors.push(author_node_error(
                text,
                &spec,
                index,
                LoomAuthorValidationCode::InvalidField,
                format!(
                    "node task must be one quote-free line of at most {LOOM_TASK_MAX_BYTES} bytes"
                ),
                "task",
            ));
        }
        for (field, type_expr) in [
            ("in_type", node.in_type.trim()),
            ("out_type", node.out_type.trim()),
        ] {
            if !valid_type_expr(type_expr) {
                errors.push(author_node_error(
                    text,
                    &spec,
                    index,
                    LoomAuthorValidationCode::InvalidField,
                    format!("node {field} must be a bounded type expression"),
                    field,
                ));
            }
        }
        if index == 0 && !node.depends_on.is_empty() {
            errors.push(author_node_error(
                text,
                &spec,
                index,
                LoomAuthorValidationCode::InvalidGraph,
                "the first node cannot depend on another node",
                "depends_on",
            ));
        }
        if index > 0 && node.depends_on.is_empty() {
            errors.push(author_node_error(
                text,
                &spec,
                index,
                LoomAuthorValidationCode::MissingField,
                "every non-root node must declare depends_on",
                "depends_on",
            ));
        }
        let signature = if let Some(agent_type) = node.agent_type.as_deref() {
            match lookup(agent_type) {
                Some(signature) => {
                    if node.in_type.trim() != signature.in_type
                        || node.out_type.trim() != signature.out_type
                    {
                        errors.push(author_node_error(
                            text,
                            &spec,
                            index,
                            LoomAuthorValidationCode::TypeMismatch,
                            format!(
                                "@{agent_type} is typed {} -> {}, not {} -> {}",
                                signature.in_type,
                                signature.out_type,
                                node.in_type.trim(),
                                node.out_type.trim()
                            ),
                            "agent_type",
                        ));
                    }
                    Some(signature)
                }
                None => {
                    errors.push(author_node_error(
                        text,
                        &spec,
                        index,
                        LoomAuthorValidationCode::UnknownAgentType,
                        format!("agent type `@{agent_type}` is not registered"),
                        "agent_type",
                    ));
                    None
                }
            }
        } else {
            None
        };
        let carries_merge = node.depends_on.len() > 1
            || node
                .depends_on
                .iter()
                .any(|dependency| authored_merged_outputs.contains(dependency.as_str()));
        let incoming = if node.depends_on.is_empty() {
            spec.in_type.clone()
        } else {
            merge_type_exprs(
                &node
                    .depends_on
                    .iter()
                    .filter_map(|dependency| authored_outputs.get(dependency.as_str()).cloned())
                    .collect::<Vec<_>>(),
            )
        };
        let accepts_incoming = if signature.is_none() {
            node.in_type == incoming
        } else if carries_merge {
            same_type_operands(&node.in_type, &incoming)
        } else {
            accepts(&node.in_type, &incoming)
        };
        if !incoming.is_empty() && !accepts_incoming {
            errors.push(author_node_error(
                text,
                &spec,
                index,
                LoomAuthorValidationCode::TypeMismatch,
                format!(
                    "node `{}` receives `{incoming}`, not authored type `{}`",
                    node.id, node.in_type
                ),
                "in_type",
            ));
        }
        let derived_output = signature
            .as_ref()
            .map_or_else(|| incoming.clone(), |signature| signature.out_type.clone());
        if !derived_output.is_empty() && node.out_type != derived_output {
            errors.push(author_node_error(
                text,
                &spec,
                index,
                LoomAuthorValidationCode::TypeMismatch,
                format!(
                    "node `{}` produces `{derived_output}`, not authored type `{}`",
                    node.id, node.out_type
                ),
                "out_type",
            ));
        }
        if signature.is_none() && carries_merge {
            authored_merged_outputs.insert(node.id.as_str());
        }
        authored_outputs.insert(node.id.as_str(), derived_output);
        validate_author_evidence(text, &spec, index, &mut errors);

        source.push('\n');
        source.push_str(&node.id);
        if let Some(agent_type) = node.agent_type.as_deref() {
            source.push_str(" @");
            source.push_str(agent_type);
        }
        if !node.task.is_empty() {
            source.push_str(" \"");
            source.push_str(&node.task);
            source.push('"');
        }
        match node.gate.as_str() {
            "command" => {}
            "review" => source.push_str(" :ship"),
            "human" => source.push_str(" :human"),
            "all_of" => {
                if let Some(evidence) = &node.evidence {
                    source.push_str(" :all-of-");
                    source.push_str(&evidence.required_green.to_string());
                }
            }
            _ => errors.push(author_error(
                text,
                LoomAuthorValidationCode::InvalidField,
                "gate must be command, review, human, or all_of",
                &format!("{base}.gate"),
                Some(&node.gate),
            )),
        }
        if index > 0 && !node.depends_on.is_empty() {
            source.push_str(" <-");
            source.push_str(&node.depends_on.join(","));
        }
        if let Some(back_edge) = node.back_edge.as_deref() {
            source.push_str(" ↺");
            source.push_str(back_edge);
        }
    }
    if errors.is_empty() {
        let ast = parse_pipe(&source);
        match compile_pipe(&ast, lookup) {
            Ok(workflow) => source = workflow.source,
            Err(compile_errors) => {
                for message in compile_errors {
                    let node_index = spec
                        .nodes
                        .iter()
                        .position(|node| compiler_error_names_node(&message, &node.id));
                    if let Some(index) = node_index {
                        let type_mismatch = message.contains("type mismatch");
                        let field = if type_mismatch {
                            "in_type"
                        } else if message.starts_with('↺') || message.starts_with("back target ")
                        {
                            "back_edge"
                        } else {
                            "depends_on"
                        };
                        errors.push(author_node_error(
                            text,
                            &spec,
                            index,
                            if type_mismatch {
                                LoomAuthorValidationCode::TypeMismatch
                            } else {
                                LoomAuthorValidationCode::InvalidGraph
                            },
                            message,
                            field,
                        ));
                    } else {
                        let output_mismatch = message.starts_with("pipe declares output ");
                        errors.push(author_error(
                            text,
                            if output_mismatch {
                                LoomAuthorValidationCode::TypeMismatch
                            } else {
                                LoomAuthorValidationCode::InvalidGraph
                            },
                            message,
                            if output_mismatch { "out_type" } else { "nodes" },
                            None,
                        ));
                    }
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let canonical_text =
        serde_json::to_string_pretty(&LoomAuthorSpec::Workflow(spec)).map_err(|error| {
            vec![author_error(
                text,
                LoomAuthorValidationCode::InvalidField,
                format!("cannot canonicalize workflow: {error}"),
                "$",
                None,
            )]
        })?;
    if canonical_text.len() > LOOM_AUTHOR_TEXT_MAX_BYTES {
        return Err(vec![author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            format!("canonical authoring text exceeds {LOOM_AUTHOR_TEXT_MAX_BYTES} bytes"),
            "$",
            None,
        )]);
    }
    Ok(ValidatedLoomAuthorSpec::Workflow {
        source,
        canonical_text,
    })
}

fn compiler_error_names_node(message: &str, node_id: &str) -> bool {
    message
        .strip_prefix("node ")
        .and_then(|tail| tail.split_once(':'))
        .is_some_and(|(name, _)| name == node_id)
        || message
            .strip_prefix("type mismatch at ")
            .and_then(|tail| tail.split_once(':'))
            .is_some_and(|(name, _)| name == node_id)
        || message.split_once(" on ").is_some_and(|(_, tail)| {
            tail.strip_prefix(node_id).is_some_and(|suffix| {
                suffix.is_empty() || suffix.starts_with(':') || suffix.starts_with(' ')
            })
        })
}

fn validate_author_evidence(
    text: &str,
    spec: &LoomAuthorWorkflowSpec,
    index: usize,
    errors: &mut Vec<LoomAuthorValidationError>,
) {
    let node = &spec.nodes[index];
    if node.gate == "human" {
        if node.evidence.is_some() {
            errors.push(author_node_error(
                text,
                spec,
                index,
                LoomAuthorValidationCode::CapabilityContradiction,
                "human gates do not consume InstructPipe evidence",
                "evidence",
            ));
        }
        return;
    }
    let Some(evidence) = &node.evidence else {
        errors.push(author_node_error(
            text,
            spec,
            index,
            LoomAuthorValidationCode::MissingField,
            "non-human gates require an InstructPipe evidence contract",
            "evidence",
        ));
        return;
    };
    if evidence.protocol != "instruct_pipe_v1" {
        errors.push(author_node_error(
            text,
            spec,
            index,
            LoomAuthorValidationCode::InvalidField,
            "evidence protocol must be instruct_pipe_v1",
            "evidence.protocol",
        ));
    }
    if evidence.tool != "graph_evidence" {
        errors.push(author_node_error(
            text,
            spec,
            index,
            LoomAuthorValidationCode::InvalidField,
            "evidence tool must be graph_evidence",
            "evidence.tool",
        ));
    }
    let required_ok = match node.gate.as_str() {
        "command" | "review" => evidence.required_green == 1,
        "all_of" => {
            evidence.required_green > 0 && evidence.required_green <= GRAPH_MAX_EVIDENCE_PER_ATTEMPT
        }
        _ => true,
    };
    if !required_ok {
        errors.push(author_node_error(
            text,
            spec,
            index,
            LoomAuthorValidationCode::InvalidField,
            format!(
                "gate `{}` has an invalid required_green value {}",
                node.gate, evidence.required_green
            ),
            "evidence.required_green",
        ));
    }
}

fn validate_capability_list(
    text: &str,
    field: &str,
    values: &[String],
    errors: &mut Vec<LoomAuthorValidationError>,
) {
    if values.len() > 32 {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "capability lists are bounded to 32 entries",
            field,
            None,
        ));
    }
    let mut seen = HashSet::new();
    for value in values {
        if !valid_capability_key(value) {
            errors.push(author_error(
                text,
                LoomAuthorValidationCode::InvalidField,
                format!("invalid capability key `{value}`; use cli:<program> or api:<host>"),
                field,
                Some(value),
            ));
        } else if !seen.insert(value) {
            errors.push(author_error(
                text,
                LoomAuthorValidationCode::DuplicateValue,
                format!("capability key `{value}` is duplicated"),
                field,
                Some(value),
            ));
        }
    }
}

fn validate_bounded_list(
    text: &str,
    field: &str,
    values: &[String],
    errors: &mut Vec<LoomAuthorValidationError>,
) {
    if values.len() > 32
        || values.iter().any(|value| {
            value.is_empty() || value.len() > 128 || value.chars().any(char::is_control)
        })
    {
        errors.push(author_error(
            text,
            LoomAuthorValidationCode::InvalidField,
            "list must have at most 32 entries of 1..=128 bytes without control characters",
            field,
            None,
        ));
    }
}

fn normalize_capability_key(value: &str) -> String {
    let value = value.trim();
    value.strip_prefix("api:").map_or_else(
        || value.to_owned(),
        |host| format!("api:{}", host.to_ascii_lowercase()),
    )
}

fn valid_capability_key(value: &str) -> bool {
    value.strip_prefix("cli:").map_or_else(
        || {
            value
                .strip_prefix("api:")
                .is_some_and(valid_author_api_host)
        },
        |program| {
            !program.is_empty()
                && program.len() <= 128
                && program.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'/')
                })
        },
    )
}

fn valid_author_api_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 128
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn valid_author_cli_grant(program: &str) -> bool {
    const DISPATCHERS: [&str; 26] = [
        ".", "source", "eval", "exec", "command", "builtin", "env", "xargs", "sh", "bash", "zsh",
        "dash", "ksh", "csh", "tcsh", "fish", "nohup", "time", "nice", "sudo", "doas", "su",
        "setsid", "stdbuf", "busybox", "toybox",
    ];
    !program.starts_with('-')
        && !program.is_empty()
        && program.len() <= 128
        && program.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'/')
        })
        && (!program.contains('/') || program.starts_with('/'))
        && !program.contains("//")
        && !program
            .split('/')
            .any(|component| matches!(component, "." | ".."))
        && program.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && program
            .rsplit('/')
            .next()
            .is_some_and(|base| !DISPATCHERS.contains(&base))
}

fn author_invisible_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

fn author_node_error(
    text: &str,
    _spec: &LoomAuthorWorkflowSpec,
    index: usize,
    code: LoomAuthorValidationCode,
    message: impl Into<String>,
    field: &str,
) -> LoomAuthorValidationError {
    let path = format!("nodes[{index}].{field}");
    author_error(text, code, message, &path, None)
}

fn author_error(
    text: &str,
    code: LoomAuthorValidationCode,
    message: impl Into<String>,
    field: &str,
    needle: Option<&str>,
) -> LoomAuthorValidationError {
    let location = needle
        .and_then(|needle| json_value_location(text, field, needle))
        .or_else(|| json_field_location(text, field))
        .or_else(|| json_parent_location(text, field))
        .unwrap_or((1, 1));
    LoomAuthorValidationError {
        code,
        message: message.into(),
        location: LoomAuthorLocation {
            line: location.0,
            column: location.1,
            field: field.to_owned(),
        },
    }
}

fn json_field_location(text: &str, target: &str) -> Option<(u32, u32)> {
    json_location(text, target, None)
}

fn json_value_location(text: &str, target: &str, needle: &str) -> Option<(u32, u32)> {
    json_location(text, target, Some(needle))
}

fn json_parent_location(text: &str, target: &str) -> Option<(u32, u32)> {
    let mut parent = target;
    while let Some((candidate, _)) = parent.rsplit_once('.') {
        if let Some(location) = json_field_location(text, candidate) {
            return Some(location);
        }
        parent = candidate;
    }
    None
}

fn json_location(text: &str, target: &str, needle: Option<&str>) -> Option<(u32, u32)> {
    if target == "$" {
        return Some((1, 1));
    }
    let mut scanner = JsonLocationScanner {
        bytes: text.as_bytes(),
        offset: 0,
        line: 1,
        column: 1,
        target,
        needle,
        found: None,
    };
    scanner.scan_value("");
    scanner.found
}

/// A total, allocation-bounded walk over already-valid authoring JSON. It
/// records object-key coordinates by their dotted/indexed path; malformed
/// input is rejected by serde before this helper is reached.
struct JsonLocationScanner<'a> {
    bytes: &'a [u8],
    offset: usize,
    line: u32,
    column: u32,
    target: &'a str,
    needle: Option<&'a str>,
    found: Option<(u32, u32)>,
}

impl JsonLocationScanner<'_> {
    fn scan_value(&mut self, path: &str) {
        self.skip_whitespace();
        let value_location = (self.line, self.column);
        if self.found.is_none() && self.needle.is_none() && path == self.target {
            self.found = Some(value_location);
        }
        match self.peek() {
            Some(b'{') => self.scan_object(path),
            Some(b'[') => self.scan_array(path),
            Some(b'"') => {
                if let Some(value) = self.scan_string()
                    && self.found.is_none()
                    && self.needle == Some(value.as_str())
                    && json_value_path_matches(path, self.target)
                {
                    self.found = Some(value_location);
                }
            }
            Some(_) => {
                while self
                    .peek()
                    .is_some_and(|byte| !byte.is_ascii_whitespace() && !b",]}".contains(&byte))
                {
                    self.advance();
                }
            }
            None => {}
        }
    }

    fn scan_object(&mut self, path: &str) {
        self.advance();
        loop {
            self.skip_whitespace();
            if self.peek() == Some(b'}') {
                self.advance();
                return;
            }
            let key_location = (self.line, self.column);
            let Some(key) = self.scan_string() else {
                return;
            };
            let child = if path.is_empty() {
                key
            } else {
                format!("{path}.{key}")
            };
            if self.found.is_none() && self.needle.is_none() && child == self.target {
                self.found = Some(key_location);
            }
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return;
            }
            self.advance();
            self.scan_value(&child);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.advance(),
                Some(b'}') => {
                    self.advance();
                    return;
                }
                _ => return,
            }
        }
    }

    fn scan_array(&mut self, path: &str) {
        self.advance();
        let mut index = 0_usize;
        loop {
            self.skip_whitespace();
            if self.peek() == Some(b']') {
                self.advance();
                return;
            }
            self.scan_value(&format!("{path}[{index}]"));
            index = index.saturating_add(1);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.advance(),
                Some(b']') => {
                    self.advance();
                    return;
                }
                _ => return,
            }
        }
    }

    fn scan_string(&mut self) -> Option<String> {
        if self.peek() != Some(b'"') {
            return None;
        }
        let start = self.offset;
        self.advance();
        let mut escaped = false;
        while let Some(byte) = self.peek() {
            self.advance();
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                return serde_json::from_slice(&self.bytes[start..self.offset]).ok();
            }
        }
        None
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.advance();
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn advance(&mut self) {
        let Some(byte) = self.peek() else {
            return;
        };
        self.offset = self.offset.saturating_add(1);
        if byte == b'\n' {
            self.line = self.line.saturating_add(1);
            self.column = 1;
        } else {
            self.column = self.column.saturating_add(1);
        }
    }
}

fn json_value_path_matches(path: &str, target: &str) -> bool {
    if path == target {
        return true;
    }
    path.strip_prefix(target).is_some_and(|suffix| {
        suffix.starts_with('[')
            && suffix.ends_with(']')
            && suffix[1..suffix.len().saturating_sub(1)]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
    })
}

/// Parse pipe source into an AST. Total: never panics, never throws — every
/// problem lands in `ast.errors`.
#[must_use]
pub fn parse_pipe(source: &str) -> LoomAst {
    let mut ast = LoomAst {
        name: None,
        in_type: String::new(),
        out_type: String::new(),
        nodes: Vec::new(),
        errors: Vec::new(),
    };
    if source.len() > LOOM_SOURCE_MAX_BYTES {
        ast.errors
            .push(format!("pipe source exceeds {LOOM_SOURCE_MAX_BYTES} bytes"));
        return ast;
    }
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Header: `name: InType -> OutType` — accepted only before any node.
        if ast.name.is_none()
            && ast.nodes.is_empty()
            && let Some((head, io)) = line.split_once(':')
            && let Some((input, output)) = io.split_once("->")
            && is_ident_chars(head.trim())
        {
            let head = head.trim();
            let input = input.trim();
            let output = output.trim();
            if head.len() > 64 {
                ast.errors
                    .push(format!("workflow name `{head}` exceeds 64 bytes"));
            }
            // Empty or malformed types would silently disable the typed-edge
            // law; a second `->` means the header was mis-split.
            if output.contains("->") {
                ast.errors.push("header declares more than one `->`".into());
            } else if !valid_type_expr(input) || !valid_type_expr(output) {
                ast.errors.push(format!(
                    "header types must be identifiers (optionally `A + B`): `{input}` -> `{output}`"
                ));
            }
            ast.name = Some(head.to_string());
            ast.in_type = input.to_string();
            ast.out_type = output.to_string();
            continue;
        }
        match parse_node_line(line) {
            Ok(node) => ast.nodes.push(node),
            Err(error) => ast.errors.push(error),
        }
    }
    if ast.name.is_none() {
        ast.errors
            .push("missing header line `name: InType -> OutType`".into());
    }
    if ast.nodes.is_empty() {
        ast.errors.push("pipe declares no nodes".into());
    }
    // Green dependencies and red back-edges must target EARLIER nodes. Keeping
    // pipe source topologically ordered makes local parsing/compilation total
    // and gives the dependency engine one deterministic declaration order.
    for (index, node) in ast.nodes.iter().enumerate() {
        if let Some(dependencies) = &node.depends_on {
            for dependency in dependencies {
                match ast.nodes.iter().position(|other| other.name == *dependency) {
                    None => ast.errors.push(format!(
                        "dependency {dependency} on {} targets no node",
                        node.name
                    )),
                    Some(target) if target >= index => ast.errors.push(format!(
                        "dependency {dependency} on {} must target an earlier node",
                        node.name
                    )),
                    Some(_) => {}
                }
            }
        }
        let Some(back) = node.back.as_deref() else {
            continue;
        };
        match ast.nodes.iter().position(|other| other.name == back) {
            None => ast
                .errors
                .push(format!("↺{back} on {} targets no node", node.name)),
            Some(target) if target > index => ast.errors.push(format!(
                "↺{back} on {} must target an earlier node",
                node.name
            )),
            Some(_) => {}
        }
    }
    ast
}

fn is_ident_chars(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn is_ident(value: &str) -> bool {
    is_ident_chars(value) && value.len() <= 64
}

/// The pipe/v1 type-expression law, shared with agent-type registration: one
/// identifier or an `A + B + ...` composite of bounded (≤64B) identifiers.
#[must_use]
pub fn valid_type_expr(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.split('+').count() <= 8
        && value.split('+').all(|operand| is_ident(operand.trim()))
}

fn parse_node_line(line: &str) -> Result<LoomNodeAst, String> {
    // The quoted task is lifted first so its words never read as tokens.
    let (task, rest) = match line.find('"') {
        Some(open) => {
            let after = &line[open + 1..];
            let Some(close) = after.find('"') else {
                return Err(format!("unterminated task quote: {line}"));
            };
            let task = after[..close].to_string();
            if task.len() > LOOM_TASK_MAX_BYTES {
                return Err(format!(
                    "task on line `{line}` exceeds {LOOM_TASK_MAX_BYTES} bytes"
                ));
            }
            let mut rest = line[..open].to_string();
            rest.push(' ');
            rest.push_str(&after[close + 1..]);
            (task, rest)
        }
        None => (String::new(), line.to_string()),
    };
    let mut tokens = rest.split_whitespace();
    let Some(name) = tokens.next() else {
        return Err(format!("unparsable line: {line}"));
    };
    if !is_ident(name) {
        return Err(format!("bad node name: {name}"));
    }
    let mut node = LoomNodeAst {
        name: name.to_string(),
        agent_type: None,
        task,
        gate: LoomGate::Cmd,
        depends_on: None,
        back: None,
    };
    let mut saw_gate = false;
    for token in tokens {
        if let Some(atype) = token.strip_prefix('@') {
            if !is_ident(atype) {
                return Err(format!("bad agent type `@{atype}` on {name}"));
            }
            // Duplicates never last-win: a second token is a contradiction.
            if node.agent_type.is_some() {
                return Err(format!("duplicate agent type on {name}"));
            }
            node.agent_type = Some(atype.to_string());
        } else if let Some(gate) = token.strip_prefix(':') {
            if saw_gate {
                return Err(format!("duplicate gate on {name}"));
            }
            saw_gate = true;
            node.gate = match gate {
                "cmd" => LoomGate::Cmd,
                "ship" => LoomGate::Ship,
                "human" => LoomGate::Human,
                other => match other.strip_prefix("all-of-") {
                    // Digits only (no `+6`), and bounded: the gate's green
                    // count must be satisfiable within one attempt's evidence
                    // budget — a wider N is a never-green node by construction.
                    Some(digits)
                        if !digits.is_empty()
                            && digits.bytes().all(|byte| byte.is_ascii_digit()) =>
                    {
                        match digits.parse::<u32>() {
                            Ok(n) if n > 0 && n <= GRAPH_MAX_EVIDENCE_PER_ATTEMPT => {
                                LoomGate::AllOf(n)
                            }
                            _ => {
                                return Err(format!(
                                    "gate :all-of-{digits} on {name} must be 1..={GRAPH_MAX_EVIDENCE_PER_ATTEMPT}"
                                ));
                            }
                        }
                    }
                    _ => return Err(format!("unknown gate :{other} on {name}")),
                },
            };
        } else if let Some(dependencies) = token.strip_prefix("<-") {
            if node.depends_on.is_some() {
                return Err(format!("duplicate dependency clause on {name}"));
            }
            let mut parsed = Vec::new();
            for dependency in dependencies.split(',') {
                if !is_ident(dependency) {
                    return Err(format!("bad dependency target `{dependency}` on {name}"));
                }
                if parsed.iter().any(|prior| prior == dependency) {
                    return Err(format!("duplicate dependency {dependency} on {name}"));
                }
                parsed.push(dependency.to_owned());
            }
            node.depends_on = Some(parsed);
        } else if token == "↻" {
            if node.back.is_some() {
                return Err(format!("duplicate red traversal on {name}"));
            }
            node.back = Some(name.to_string());
        } else if let Some(back) = token.strip_prefix('↺').or_else(|| token.strip_prefix('^')) {
            if !is_ident(back) {
                return Err(format!("bad back-edge target `{back}` on {name}"));
            }
            if node.back.is_some() {
                return Err(format!("duplicate red traversal on {name}"));
            }
            node.back = Some(back.to_string());
        } else {
            return Err(format!("unexpected token `{token}` on {name}"));
        }
    }
    Ok(node)
}

/// Compile a clean AST into a registrable workflow. The `lookup` resolves
/// `@agent-type` references against the Loom registry. Fails with the FULL
/// error list (parse errors included) — registration rejects, runs never see
/// a half-built graph.
pub fn compile_pipe(
    ast: &LoomAst,
    lookup: impl Fn(&str) -> Option<LoomTypeSig>,
) -> Result<LoomWorkflow, Vec<String>> {
    let mut errors = ast.errors.clone();
    let name = ast.name.clone().unwrap_or_default();

    // Node identities: uppercase the source names onto GraphNodeName. Artifact
    // types travel along the same immutable dependency edges as readiness.
    let mut meta = Vec::new();
    let mut specs = Vec::new();
    let mut previous: Option<GraphNodeName> = None;
    let mut source_nodes = HashMap::<String, GraphNodeName>::new();
    let mut node_outputs = HashMap::<GraphNodeName, String>::new();
    let mut merged_outputs = HashSet::<GraphNodeName>::new();
    for node in &ast.nodes {
        let upper = node.name.to_ascii_uppercase();
        let cg_name = match GraphNodeName::new(upper) {
            Ok(cg) => cg,
            Err(error) => {
                errors.push(format!("node {}: {error}", node.name));
                continue;
            }
        };
        let dependencies = match &node.depends_on {
            Some(authored) => {
                let mut resolved = Vec::new();
                for dependency in authored {
                    let Some(cg_dependency) = source_nodes.get(dependency) else {
                        // `parse_pipe` reports the more specific unknown/forward
                        // distinction. Keep compile total for callers that build
                        // a LoomAst directly instead of using the parser.
                        errors.push(format!(
                            "dependency {dependency} on {} must target an earlier node",
                            node.name
                        ));
                        continue;
                    };
                    if resolved.iter().any(|prior| prior == cg_dependency) {
                        errors.push(format!(
                            "duplicate dependency {dependency} on {}",
                            node.name
                        ));
                        continue;
                    }
                    resolved.push(cg_dependency.clone());
                }
                resolved
            }
            None => previous.clone().into_iter().collect(),
        };
        let incoming = if dependencies.is_empty() {
            vec![ast.in_type.clone()]
        } else {
            dependencies
                .iter()
                .filter_map(|dependency| node_outputs.get(dependency).cloned())
                .collect::<Vec<_>>()
        };
        let carried = merge_type_exprs(&incoming);
        let carries_merge = dependencies.len() > 1
            || dependencies
                .iter()
                .any(|dependency| merged_outputs.contains(dependency));
        let signature = match node.agent_type.as_deref() {
            Some(atype) => match lookup(atype) {
                Some(signature) => Some(signature),
                None => {
                    errors.push(format!(
                        "node {} references unregistered agent type @{atype}",
                        node.name
                    ));
                    None
                }
            },
            None => None,
        };
        // A4 — the typed-edge law. One incoming edge retains pipe/v1's exact-
        // or-one-composite-operand widening. A real JOIN is strict: its entire
        // merged input expression must equal the specialist's input expression
        // modulo operand order. Missing and extra branch artifacts both reject.
        if let Some(signature) = &signature
            && !carried.is_empty()
        {
            let accepted = if carries_merge {
                same_type_operands(&signature.in_type, &carried)
            } else {
                accepts(&signature.in_type, &carried)
            };
            if !accepted {
                let input = if carries_merge {
                    "merged inputs"
                } else {
                    "carries"
                };
                errors.push(format!(
                    "type mismatch at {}: {input} `{carried}` but @{} accepts `{}`",
                    node.name,
                    node.agent_type.as_deref().unwrap_or("?"),
                    signature.in_type
                ));
            }
        }
        // A target like `2nd` passes ident parsing but is no legal CG name.
        let back = match node.back.as_deref() {
            Some(target) => match GraphNodeName::new(target.to_ascii_uppercase()) {
                Ok(cg) => Some(cg),
                Err(error) => {
                    errors.push(format!("back target {target} on {}: {error}", node.name));
                    None
                }
            },
            None => None,
        };
        let (gate, executor, max_attempts) = match node.gate {
            LoomGate::Cmd | LoomGate::Ship => (
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                LOOM_WORK_MAX_ATTEMPTS,
            ),
            LoomGate::AllOf(n) => (
                GraphGateKind::AllOfN { n },
                GraphExecutorShape::FanOut,
                LOOM_FANOUT_MAX_ATTEMPTS,
            ),
            LoomGate::Human => (
                GraphGateKind::HumanConfirm,
                GraphExecutorShape::Human,
                GRAPH_MAX_ATTEMPTS,
            ),
        };
        // The store's evidence path REQUIRES a round bound on every
        // non-human gate ("open graph node has no evidence-round bound" is a
        // StoreCorrupt); human gates must not carry one.
        let max_evidence_per_attempt =
            (gate != GraphGateKind::HumanConfirm).then_some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT);
        specs.push(GraphNodeSpec {
            name: cg_name.clone(),
            gate,
            executor,
            max_attempts,
            max_evidence_per_attempt,
            depends_on: dependencies,
            red_target: back.clone(),
            verify_slots: Vec::new(),
        });
        meta.push(LoomNodeMeta {
            source_name: node.name.clone(),
            node: cg_name.clone(),
            agent_type: node.agent_type.clone(),
            agent_type_rev: None,
            agent_type_digest: None,
            task: node.task.clone(),
            in_type: signature.as_ref().map(|s| s.in_type.clone()),
            out_type: signature.as_ref().map(|s| s.out_type.clone()),
            back,
        });
        // A control node is identity on its complete input, including a merged
        // join input. Work nodes replace that input with their declared output.
        let control_node = signature.is_none();
        let output = signature
            .as_ref()
            .map_or(carried, |signature| signature.out_type.clone());
        if control_node && carries_merge {
            merged_outputs.insert(cg_name.clone());
        }
        node_outputs.insert(cg_name.clone(), output);
        source_nodes.insert(node.name.clone(), cg_name.clone());
        previous = Some(cg_name);
    }
    // A DAG may have multiple terminal branches. Its effective output is the
    // strict merge of every terminal artifact; a linear graph still has the
    // exact byte-for-byte behavior of its single last carried artifact.
    if errors.is_empty() && !ast.out_type.is_empty() {
        let depended_on = specs
            .iter()
            .flat_map(|spec| spec.depends_on.iter().cloned())
            .collect::<HashSet<_>>();
        let terminal_outputs = specs
            .iter()
            .filter(|spec| !depended_on.contains(&spec.name))
            .filter_map(|spec| node_outputs.get(&spec.name).cloned())
            .collect::<Vec<_>>();
        let produced = merge_type_exprs(&terminal_outputs);
        let output_is_merged = terminal_outputs.len() > 1
            || specs.iter().any(|spec| {
                !depended_on.contains(&spec.name) && merged_outputs.contains(&spec.name)
            });
        let output_matches = if output_is_merged {
            same_type_operands(&ast.out_type, &produced)
        } else {
            accepts(&ast.out_type, &produced)
        };
        if !output_matches {
            errors.push(format!(
                "pipe declares output `{}` but its nodes produce `{produced}`",
                ast.out_type
            ));
        }
    }
    let template = GraphTemplateSpec {
        name: name.clone(),
        version: GRAPH_TEMPLATE_VERSION,
        start_node: specs.first().map(|spec| spec.name.clone()),
        nodes: specs,
    };
    if errors.is_empty()
        && let Err(error) = validate_graph_template(&template)
    {
        errors.push(format!("template rejected: {}", error.message));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut workflow = LoomWorkflow {
        id: name,
        pipe_version: LOOM_PIPE_VERSION.into(),
        source: rebuild_source(ast),
        in_type: ast.in_type.clone(),
        out_type: ast.out_type.clone(),
        template,
        meta,
        rev: 1,
        digest: String::new(),
    };
    workflow.digest = workflow_digest(&workflow);
    Ok(workflow)
}

/// `expected` accepts `carried` when they match exactly, or when `expected`
/// is a `A + B` composite and `carried` is one of its operands.
fn accepts(expected: &str, carried: &str) -> bool {
    if expected == carried {
        return true;
    }
    expected
        .split('+')
        .map(str::trim)
        .any(|operand| operand == carried)
}

/// Merge artifact expressions as a stable union. The first dependency's
/// operand order wins for rendering/error text; duplicate type names collapse
/// because pipe/v1's type vocabulary has no labels or multiplicity.
fn merge_type_exprs(inputs: &[String]) -> String {
    if let [only] = inputs {
        return only.clone();
    }
    let mut operands = Vec::<&str>::new();
    for operand in inputs
        .iter()
        .flat_map(|input| input.split('+').map(str::trim))
        .filter(|operand| !operand.is_empty())
    {
        if !operands.contains(&operand) {
            operands.push(operand);
        }
    }
    operands.join(" + ")
}

/// Composite equality at a JOIN is deliberately order-insensitive but strict
/// about membership: `A + B` accepts `B + A`, not `A` or `A + B + C`.
fn same_type_operands(expected: &str, carried: &str) -> bool {
    normalized_type_operands(expected) == normalized_type_operands(carried)
}

fn normalized_type_operands(value: &str) -> Vec<&str> {
    let mut operands = value
        .split('+')
        .map(str::trim)
        .filter(|operand| !operand.is_empty())
        .collect::<Vec<_>>();
    operands.sort_unstable();
    operands.dedup();
    operands
}

/// Canonical source: the AST printed back in one normal form, so the digest
/// is insensitive to author whitespace.
fn rebuild_source(ast: &LoomAst) -> String {
    let mut out = format!(
        "{}: {} -> {}",
        ast.name.as_deref().unwrap_or(""),
        ast.in_type,
        ast.out_type
    );
    for node in &ast.nodes {
        out.push('\n');
        out.push_str(&node.name);
        if let Some(atype) = &node.agent_type {
            out.push_str(" @");
            out.push_str(atype);
        }
        if !node.task.is_empty() {
            out.push_str(" \"");
            out.push_str(&node.task);
            out.push('"');
        }
        match node.gate {
            LoomGate::Cmd => {}
            LoomGate::Ship => out.push_str(" :ship"),
            LoomGate::AllOf(n) => {
                out.push_str(" :all-of-");
                out.push_str(&n.to_string());
            }
            LoomGate::Human => out.push_str(" :human"),
        }
        if let Some(dependencies) = &node.depends_on {
            out.push_str(" <-");
            out.push_str(&dependencies.join(","));
        }
        if let Some(back) = &node.back {
            if back == &node.name {
                out.push_str(" ↻");
            } else {
                out.push_str(" ↺");
                out.push_str(back);
            }
        }
    }
    out
}

fn workflow_digest(workflow: &LoomWorkflow) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut part = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    part(workflow.pipe_version.as_bytes());
    part(workflow.source.as_bytes());
    // The digest binds the RESOLVED type signatures too: the same source
    // compiled against a changed registry is a different workflow identity.
    for meta in &workflow.meta {
        part(meta.node.as_str().as_bytes());
        part(meta.in_type.as_deref().unwrap_or("").as_bytes());
        part(meta.out_type.as_deref().unwrap_or("").as_bytes());
        if let Some(rev) = meta.agent_type_rev {
            part(&rev.to_le_bytes());
        }
        if let Some(digest) = meta.agent_type_digest.as_deref() {
            part(digest.as_bytes());
        }
    }
    hasher.finalize().to_hex()[..32].to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn registry() -> HashMap<&'static str, LoomTypeSig> {
        let sig = |i: &str, o: &str| LoomTypeSig {
            in_type: i.into(),
            out_type: o.into(),
        };
        HashMap::from([
            ("researcher", sig("SourceURL", "Transcript")),
            ("proposer", sig("Transcript", "VideoProposal")),
            ("capturer", sig("VideoProposal", "AssetBundle")),
            ("editor", sig("VideoProposal + AssetBundle", "Timeline")),
            ("renderer", sig("Timeline", "VideoFile")),
        ])
    }

    const MAKE_VIDEO: &str = r#"
# the flagship
make-video: SourceURL -> VideoFile
research @researcher "pull the source and transcribe it" :cmd
propose  @proposer   "shape a hook and a 6-beat arc"     :ship
capture  @capturer   "gather b-roll for every beat"      :all-of-6 ↺propose
edit     @editor     "cut to the arc, trim dead air"     :ship <-propose,capture ^capture
render   @renderer   "encode 1080p H.264"                :cmd
publish              "you approve the cut"               :human
"#;

    /// MUTATION CHECK: drop a gate mapping, stop uppercasing names, or skip a
    /// declared dependency. Expected RUNTIME failure below.
    #[test]
    fn make_video_parses_and_compiles_onto_cg_vocabulary() {
        let ast = parse_pipe(MAKE_VIDEO);
        assert!(ast.errors.is_empty(), "{:?}", ast.errors);
        assert_eq!(ast.name.as_deref(), Some("make-video"));
        assert_eq!(ast.in_type, "SourceURL");
        assert_eq!(ast.nodes.len(), 6);
        assert_eq!(ast.nodes[2].gate, LoomGate::AllOf(6));
        assert_eq!(ast.nodes[2].back.as_deref(), Some("propose"));
        // ASCII ^ alias reads identically to ↺.
        assert_eq!(ast.nodes[3].back.as_deref(), Some("capture"));
        assert_eq!(
            ast.nodes[3].depends_on.as_deref(),
            Some(["propose".to_owned(), "capture".to_owned()].as_slice())
        );

        let registry = registry();
        let workflow = compile_pipe(&ast, |id| registry.get(id).cloned()).expect("compiles");
        assert_eq!(workflow.pipe_version, LOOM_PIPE_VERSION);
        assert_eq!(workflow.template.nodes.len(), 6);
        assert_eq!(workflow.template.nodes[0].name.as_str(), "RESEARCH");
        assert_eq!(
            workflow.template.start_node.as_ref().map(|n| n.as_str()),
            Some("RESEARCH")
        );
        // The start is dependency-free; EDIT is the explicit typed join.
        assert!(workflow.template.nodes[0].depends_on.is_empty());
        assert_eq!(
            workflow.template.nodes[3]
                .depends_on
                .iter()
                .map(|node| node.as_str())
                .collect::<Vec<_>>(),
            ["PROPOSE", "CAPTURE"]
        );
        // Gate lowering.
        assert_eq!(
            workflow.template.nodes[2].gate,
            GraphGateKind::AllOfN { n: 6 }
        );
        assert_eq!(workflow.template.nodes[5].gate, GraphGateKind::HumanConfirm);
        assert_eq!(
            workflow.template.nodes[5].executor,
            GraphExecutorShape::Human
        );
        // Loom metadata: derived IO + back targets.
        assert_eq!(workflow.meta[0].out_type.as_deref(), Some("Transcript"));
        assert_eq!(
            workflow.meta[2].back.as_ref().map(|n| n.as_str()),
            Some("PROPOSE")
        );
        // The control node carries no type and is identity on the artifact.
        assert!(workflow.meta[5].agent_type.is_none());
        assert!(!workflow.digest.is_empty());
        // Verify-fix pin: every non-human node carries the evidence-round
        // bound the store REQUIRES (None → StoreCorrupt at run time); human
        // gates must not carry one.
        for spec in &workflow.template.nodes {
            if spec.gate == GraphGateKind::HumanConfirm {
                assert!(spec.max_evidence_per_attempt.is_none());
            } else {
                assert_eq!(
                    spec.max_evidence_per_attempt,
                    Some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT)
                );
            }
        }
        assert_eq!(workflow.template.version, GRAPH_TEMPLATE_VERSION);
        // The compiled template passes the CG validator (redundant with
        // compile, pinned here so a validator change surfaces loudly).
        assert!(validate_graph_template(&workflow.template).is_ok());
    }

    /// Explicit dependencies lower a real diamond onto CG `depends_on`. A JOIN
    /// consumes every predecessor artifact, and composite operand order is not
    /// semantically significant.
    #[test]
    fn explicit_fork_join_compiles_and_type_checks_merged_inputs() {
        let source = r#"fork-join: Seed -> Result
start "seed"
left @left "make left" <-start
right @right "make right" <-start
join @join "merge both" <-left,right"#;
        let ast = parse_pipe(source);
        assert!(ast.errors.is_empty(), "{:?}", ast.errors);
        assert_eq!(
            ast.nodes[3].depends_on.as_deref(),
            Some(["left".to_owned(), "right".to_owned()].as_slice())
        );

        let workflow = compile_pipe(&ast, |id| match id {
            "left" => Some(LoomTypeSig {
                in_type: "Seed".into(),
                out_type: "Left".into(),
            }),
            "right" => Some(LoomTypeSig {
                in_type: "Seed".into(),
                out_type: "Right".into(),
            }),
            "join" => Some(LoomTypeSig {
                // Reverse order proves JOIN comparison is order-insensitive.
                in_type: "Right + Left".into(),
                out_type: "Result".into(),
            }),
            _ => None,
        })
        .expect("fork/join compiles");
        let dependency_names = |index: usize| {
            workflow.template.nodes[index]
                .depends_on
                .iter()
                .map(|node| node.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(dependency_names(1), ["START"]);
        assert_eq!(dependency_names(2), ["START"]);
        assert_eq!(dependency_names(3), ["LEFT", "RIGHT"]);
        assert_eq!(
            workflow.source,
            "fork-join: Seed -> Result\nstart \"seed\"\nleft @left \"make left\" <-start\nright @right \"make right\" <-start\njoin @join \"merge both\" <-left,right"
        );
        assert!(validate_graph_template(&workflow.template).is_ok());
    }

    /// JOIN inputs are exact as a set: the old single-edge composite widening
    /// must not hide a missing branch or accept an artifact no branch produced.
    #[test]
    fn explicit_join_rejects_missing_and_extra_input_operands() {
        let ast = parse_pipe(
            "strict: Seed -> Result\nstart\nleft @left <-start\nright @right <-start\njoin @join <-left,right",
        );
        let compile_with_join_input = |join_input: &str| {
            compile_pipe(&ast, |id| match id {
                "left" => Some(LoomTypeSig {
                    in_type: "Seed".into(),
                    out_type: "Left".into(),
                }),
                "right" => Some(LoomTypeSig {
                    in_type: "Seed".into(),
                    out_type: "Right".into(),
                }),
                "join" => Some(LoomTypeSig {
                    in_type: join_input.into(),
                    out_type: "Result".into(),
                }),
                _ => None,
            })
            .expect_err("incoherent join input must reject")
        };
        for expected in ["Left", "Left + Right + Extra"] {
            let errors = compile_with_join_input(expected);
            assert!(
                errors
                    .iter()
                    .any(|error| error
                        .contains("type mismatch at join: merged inputs `Left + Right`")),
                "{errors:?}"
            );
        }
    }

    /// An untyped JOIN is identity on the complete merged artifact. A following
    /// typed node receives that composite through its ordinary single edge.
    #[test]
    fn control_join_is_identity_on_merged_input() {
        let ast = parse_pipe(
            "control-join: Seed -> Result\nstart\nleft @left <-start\nright @right <-start\nmerge <-left,right\nfinish @finish",
        );
        let workflow = compile_pipe(&ast, |id| match id {
            "left" => Some(LoomTypeSig {
                in_type: "Seed".into(),
                out_type: "Left".into(),
            }),
            "right" => Some(LoomTypeSig {
                in_type: "Seed".into(),
                out_type: "Right".into(),
            }),
            "finish" => Some(LoomTypeSig {
                // A control JOIN preserves merge provenance as well as value;
                // the downstream strict comparison is order-insensitive.
                in_type: "Right + Left".into(),
                out_type: "Result".into(),
            }),
            _ => None,
        })
        .expect("control join carries the merged artifact");
        assert!(workflow.meta[3].agent_type.is_none());
        assert_eq!(workflow.template.nodes[4].depends_on[0].as_str(), "MERGE");
    }

    /// With no explicit final join, the header describes the strict merge of
    /// all terminal branch outputs instead of whichever line was declared last.
    #[test]
    fn workflow_output_merges_all_terminal_branches() {
        let ast = parse_pipe(
            "terminals: Seed -> Right + Left\nstart\nleft @left <-start\nright @right <-start",
        );
        let lookup = |id: &str| match id {
            "left" => Some(LoomTypeSig {
                in_type: "Seed".into(),
                out_type: "Left".into(),
            }),
            "right" => Some(LoomTypeSig {
                in_type: "Seed".into(),
                out_type: "Right".into(),
            }),
            _ => None,
        };
        compile_pipe(&ast, lookup).expect("terminal outputs merge order-insensitively");

        let bad = parse_pipe(
            "terminals-bad: Seed -> Left\nstart\nleft @left <-start\nright @right <-start",
        );
        let errors = compile_pipe(&bad, lookup).expect_err("missing terminal type rejects");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("nodes produce `Left + Right`")),
            "{errors:?}"
        );
    }

    #[test]
    fn dependencies_must_be_unique_earlier_nodes() {
        let forward = parse_pipe("f: A -> A\na <-later\nlater");
        assert!(
            forward
                .errors
                .iter()
                .any(|error| error.contains("must target an earlier node"))
        );
        let unknown = parse_pipe("f: A -> A\na\nb <-ghost");
        assert!(
            unknown
                .errors
                .iter()
                .any(|error| error.contains("targets no node"))
        );
        let duplicate = parse_pipe("f: A -> A\na\nb <-a,a");
        assert!(
            duplicate
                .errors
                .iter()
                .any(|error| error.contains("duplicate dependency a on b"))
        );
        let duplicate_clause = parse_pipe("f: A -> A\na\nb <-a <-a");
        assert!(
            duplicate_clause
                .errors
                .iter()
                .any(|error| error.contains("duplicate dependency clause on b"))
        );
    }

    /// MUTATION CHECK: stop collecting errors (first-failure or panic), or
    /// accept forward back-edges. Expected RUNTIME failure below.
    #[test]
    fn parse_is_total_and_rejects_bad_lines_with_reasons() {
        let ast = parse_pipe(
            "flow: A -> B\nok @t \"fine\"\nbad :nope\nloop ↺later\nlater \"x\" :cmd\n\"floating\"",
        );
        assert_eq!(ast.nodes.len(), 3, "{:?}", ast.nodes);
        let joined = ast.errors.join("\n");
        assert!(joined.contains("unknown gate :nope"), "{joined}");
        assert!(
            joined.contains("↺later on loop must target an earlier node")
                || joined.contains("targets no node"),
            "{joined}"
        );
        assert!(
            joined.contains("unparsable line") || joined.contains("bad node name"),
            "{joined}"
        );

        let headless = parse_pipe("just-a-node :cmd");
        assert!(headless.errors.iter().any(|e| e.contains("missing header")));

        // Never panics on garbage.
        let _ = parse_pipe("::::\n\"\n@@@@ ↺↺");
    }

    /// MUTATION CHECK: drop the A4 type-check or the composite `A + B` rule.
    /// Expected RUNTIME failure below.
    #[test]
    fn compile_rejects_type_mismatch_and_unknown_agent_types() {
        let registry = registry();
        // renderer (Timeline in) directly after researcher (Transcript out).
        let ast = parse_pipe("bad: SourceURL -> VideoFile\na @researcher \"x\"\nb @renderer \"y\"");
        let errors = compile_pipe(&ast, |id| registry.get(id).cloned()).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("type mismatch at b")),
            "{errors:?}"
        );

        let ast = parse_pipe("bad2: A -> B\nn @ghost \"x\"");
        let errors = compile_pipe(&ast, |id| registry.get(id).cloned()).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unregistered agent type @ghost")),
            "{errors:?}"
        );

        // The declared pipe output must match what the chain produces.
        let ast = parse_pipe("bad3: SourceURL -> VideoFile\na @researcher \"x\"");
        let errors = compile_pipe(&ast, |id| registry.get(id).cloned()).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("nodes produce `Transcript`")),
            "{errors:?}"
        );
    }

    /// MUTATION CHECK: canonicalization drift — the digest must be stable
    /// across author whitespace but move when the source meaning changes.
    #[test]
    fn digest_is_whitespace_insensitive_and_meaning_sensitive() {
        let registry = registry();
        let compile = |src: &str| compile_pipe(&parse_pipe(src), |id| registry.get(id).cloned());
        let a = compile("f: SourceURL -> Transcript\nn   @researcher    \"t\"").expect("a");
        let b = compile("f: SourceURL -> Transcript\nn @researcher \"t\"").expect("b");
        assert_eq!(a.digest, b.digest);
        assert_eq!(
            a.source, "f: SourceURL -> Transcript\nn @researcher \"t\"",
            "an implicit previous edge stays implicit in canonical source"
        );
        let c = compile("f: SourceURL -> Transcript\nn @researcher \"different task\"").expect("c");
        assert_ne!(a.digest, c.digest);
    }

    /// A control-only pipe (no agent types) compiles: Loom is a superset of
    /// plain gated DAGs.
    #[test]
    fn control_only_pipe_compiles_without_a_registry() {
        let ast = parse_pipe(
            "plain: Task -> Task\nbuild \"make it\"\ncheck \"verify\" :all-of-4 ↺build\nship \"approve\" :human",
        );
        let workflow = compile_pipe(&ast, |_| None).expect("control-only compiles");
        assert_eq!(workflow.template.nodes.len(), 3);
        assert!(workflow.meta.iter().all(|m| m.agent_type.is_none()));
    }

    #[test]
    fn conditional_self_loop_lowers_to_the_immutable_graph_target() {
        let ast = parse_pipe("loop: Task -> Task\nbuild \"retry this node\" ↻");
        assert!(ast.errors.is_empty(), "{:?}", ast.errors);
        assert_eq!(ast.nodes[0].back.as_deref(), Some("build"));

        let workflow = compile_pipe(&ast, |_| None).expect("self-loop compiles");
        let build = GraphNodeName::new("BUILD").expect("valid name");
        assert_eq!(workflow.meta[0].back.as_ref(), Some(&build));
        assert_eq!(workflow.template.nodes[0].red_target.as_ref(), Some(&build));
        assert_eq!(
            workflow.source,
            "loop: Task -> Task\nbuild \"retry this node\" ↻"
        );
    }

    /// Agent-type digests: rev-sensitive, list-order-sensitive.
    #[test]
    fn agent_type_digest_moves_with_identity() {
        let base = LoomAgentType {
            id: "thumbnailer".into(),
            name: "Thumbnailer".into(),
            job: "make thumbnails".into(),
            in_type: "Prompt".into(),
            out_type: "Image".into(),
            clis: vec![],
            apis: vec!["fal.ai".into()],
            denials: Vec::new(),
            skills: vec!["nanobanana-prompting".into()],
            scripts: vec![],
            color: "#c2557a".into(),
            glyph: "✦".into(),
            rev: 1,
        };
        // The registry rev is a counter, NOT content identity: same content
        // at a bumped rev keeps its digest (that is the no-op detection law).
        let mut bumped = base.clone();
        bumped.rev = 2;
        assert_eq!(base.digest(), bumped.digest());
        let mut regranted = base.clone();
        regranted.apis.push("elevenlabs".into());
        assert_ne!(base.digest(), regranted.digest());
    }
    /// Verify-fix pins: duplicate tokens are contradictions, not last-wins;
    /// all-of-N is digits-only and bounded; headers with empty/malformed
    /// types or a second `->` reject instead of disabling the type law.
    #[test]
    fn parse_rejects_duplicates_bounds_and_bad_headers() {
        let dup = parse_pipe("f: A -> A\nn :human :cmd \"x\"");
        assert!(dup.errors.iter().any(|e| e.contains("duplicate gate")));
        let dup_type = parse_pipe("f: A -> A\nn @a @b \"x\"");
        assert!(
            dup_type
                .errors
                .iter()
                .any(|e| e.contains("duplicate agent type"))
        );
        let dup_back = parse_pipe("f: A -> A\na \"x\"\nb ↺a ^a");
        assert!(
            dup_back
                .errors
                .iter()
                .any(|e| e.contains("duplicate red traversal"))
        );

        let wide = parse_pipe("f: A -> A\nn :all-of-4294967295 \"x\"");
        assert!(wide.errors.iter().any(|e| e.contains("must be 1..=")));
        let plus = parse_pipe("f: A -> A\nn :all-of-+6 \"x\"");
        assert!(plus.errors.iter().any(|e| e.contains("unknown gate")));

        let empty_in = parse_pipe("f:  -> B\nn \"x\"");
        assert!(empty_in.errors.iter().any(|e| e.contains("header types")));
        let double_arrow = parse_pipe("f: A -> B -> C\nn \"x\"");
        assert!(
            double_arrow
                .errors
                .iter()
                .any(|e| e.contains("more than one"))
        );
        let long = parse_pipe(&format!("{}: A -> A\nn \"x\"", "x".repeat(70)));
        assert!(long.errors.iter().any(|e| e.contains("exceeds 64 bytes")));
    }

    /// Verify-fix pin: the workflow digest binds the RESOLVED registry
    /// signatures — same source, changed registry, different identity.
    #[test]
    fn digest_binds_resolved_type_signatures() {
        let src = "f: SourceURL -> Transcript\nn @researcher \"t\"";
        let a = compile_pipe(&parse_pipe(src), |_| {
            Some(LoomTypeSig {
                in_type: "SourceURL".into(),
                out_type: "Transcript".into(),
            })
        })
        .expect("a");
        // Same source, but the registry now says the researcher produces a
        // RicherTranscript — the pipe output check would fail, so compare via
        // a compatible change: keep out_type but change in_type acceptance.
        let b = compile_pipe(
            &parse_pipe("f:  .. -> Transcript\nn @researcher \"t\""),
            |_| None,
        );
        assert!(b.is_err(), "malformed header must reject");
        let c = compile_pipe(&parse_pipe(src), |_| {
            Some(LoomTypeSig {
                in_type: "SourceURL + PlaylistURL".into(),
                out_type: "Transcript".into(),
            })
        })
        .expect("c");
        assert_ne!(a.digest, c.digest);
    }
}
