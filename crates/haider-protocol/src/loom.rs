//! Loom pipe/v1 — the typed-workflow DSL and agent-type vocabulary.
//!
//! The authoring model emits **pipe source** (one terse line per node, spec:
//! `docs/design/loom-pipe-v1.md`), never a JSON graph. [`parse_pipe`] turns
//! source into an AST **totally** — errors are collected, never thrown — and
//! [`compile_pipe`] lowers the AST onto the existing Convergence-Graph
//! vocabulary ([`GraphTemplateSpec`]/[`GraphNodeSpec`]/[`GraphGateKind`]) so
//! pinning, reduction, advancement and the TUI status surfaces execute Loom
//! workflows without new graph machinery. What is genuinely new — the
//! per-node agent type, task, and derived typed I/O — rides beside the
//! template as [`LoomNodeMeta`], and the pipe source stays the structure of
//! record: the template is a derived artifact.

use crate::graph::{
    GRAPH_MAX_ATTEMPTS, GRAPH_MAX_EVIDENCE_PER_ATTEMPT, GRAPH_TEMPLATE_VERSION, GraphExecutorShape,
    GraphGateKind, GraphNodeName, GraphNodeSpec, GraphTemplateSpec, validate_graph_template,
};
use serde::{Deserialize, Serialize};

/// The DSL revision carried by every compiled workflow record.
pub const LOOM_PIPE_VERSION: &str = "pipe/v1";
/// Pipe source above this size is rejected before parsing.
pub const LOOM_SOURCE_MAX_BYTES: usize = 16 * 1024;
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
        let mut hasher = blake3::Hasher::new();
        for part in [
            self.id.as_str(),
            self.name.as_str(),
            self.job.as_str(),
            self.in_type.as_str(),
            self.out_type.as_str(),
            self.color.as_str(),
            self.glyph.as_str(),
        ] {
            hasher.update(part.as_bytes());
            hasher.update(b"\x1f");
        }
        for list in [&self.clis, &self.apis, &self.skills, &self.scripts] {
            for item in list {
                hasher.update(item.as_bytes());
                hasher.update(b"\x1e");
            }
            hasher.update(b"\x1f");
        }
        hasher.finalize().to_hex()[..16].to_string()
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
    /// Conditional red back-edge target (must name an earlier node).
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
/// (which has no vocabulary for agent types, tasks, typed I/O or back-edges).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomNodeMeta {
    /// The source-level (lowercase) name.
    pub source_name: String,
    /// The CG node identity (uppercased source name).
    pub node: GraphNodeName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
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

/// Registry outcome for one register call (agent type or workflow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomRegistration {
    pub id: String,
    pub rev: u32,
    pub digest: String,
    /// False when the call was an idempotent same-content no-op.
    pub updated: bool,
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
    // Back-edges must target an EARLIER node — the DAG never time-travels.
    for (index, node) in ast.nodes.iter().enumerate() {
        let Some(back) = node.back.as_deref() else {
            continue;
        };
        match ast.nodes.iter().position(|other| other.name == back) {
            None => ast
                .errors
                .push(format!("↺{back} on {} targets no node", node.name)),
            Some(target) if target >= index => ast.errors.push(format!(
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
    !value.is_empty() && value.split('+').all(|operand| is_ident(operand.trim()))
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
        } else if let Some(back) = token.strip_prefix('↺').or_else(|| token.strip_prefix('^')) {
            if !is_ident(back) {
                return Err(format!("bad back-edge target `{back}` on {name}"));
            }
            if node.back.is_some() {
                return Err(format!("duplicate back-edge on {name}"));
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

    // Node identities: uppercase the source names onto GraphNodeName.
    let mut meta = Vec::new();
    let mut specs = Vec::new();
    let mut previous: Option<GraphNodeName> = None;
    let mut carried = ast.in_type.clone();
    for node in &ast.nodes {
        let upper = node.name.to_ascii_uppercase();
        let cg_name = match GraphNodeName::new(upper) {
            Ok(cg) => cg,
            Err(error) => {
                errors.push(format!("node {}: {error}", node.name));
                continue;
            }
        };
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
        // A4 — the typed-edge law: a work node must accept what the pipe has
        // carried to it. `A + B` composite inputs accept any one operand.
        if let Some(signature) = &signature {
            if !carried.is_empty() && !accepts(&signature.in_type, &carried) {
                errors.push(format!(
                    "type mismatch at {}: carries `{carried}` but @{} accepts `{}`",
                    node.name,
                    node.agent_type.as_deref().unwrap_or("?"),
                    signature.in_type
                ));
            }
            carried = signature.out_type.clone();
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
            depends_on: previous.clone().into_iter().collect(),
            verify_slots: Vec::new(),
        });
        meta.push(LoomNodeMeta {
            source_name: node.name.clone(),
            node: cg_name.clone(),
            agent_type: node.agent_type.clone(),
            task: node.task.clone(),
            in_type: signature.as_ref().map(|s| s.in_type.clone()),
            out_type: signature.as_ref().map(|s| s.out_type.clone()),
            back,
        });
        previous = Some(cg_name);
    }
    // The pipe's declared output must be what the last work node produces.
    if errors.is_empty() && !ast.out_type.is_empty() && !accepts(&ast.out_type, &carried) {
        errors.push(format!(
            "pipe declares output `{}` but its nodes produce `{carried}`",
            ast.out_type
        ));
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
        if let Some(back) = &node.back {
            out.push_str(" ↺");
            out.push_str(back);
        }
    }
    out
}

fn workflow_digest(workflow: &LoomWorkflow) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(workflow.pipe_version.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(workflow.source.as_bytes());
    // The digest binds the RESOLVED type signatures too: the same source
    // compiled against a changed registry is a different workflow identity.
    for meta in &workflow.meta {
        hasher.update(meta.node.as_str().as_bytes());
        hasher.update(b"\x1e");
        hasher.update(meta.in_type.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\x1e");
        hasher.update(meta.out_type.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.finalize().to_hex()[..16].to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
edit     @editor     "cut to the arc, trim dead air"     :ship ^capture
render   @renderer   "encode 1080p H.264"                :cmd
publish              "you approve the cut"               :human
"#;

    /// MUTATION CHECK: drop a gate mapping, stop uppercasing names, or skip
    /// the linear dependency chain. Expected RUNTIME failure below.
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

        let registry = registry();
        let workflow = compile_pipe(&ast, |id| registry.get(id).cloned()).expect("compiles");
        assert_eq!(workflow.pipe_version, LOOM_PIPE_VERSION);
        assert_eq!(workflow.template.nodes.len(), 6);
        assert_eq!(workflow.template.nodes[0].name.as_str(), "RESEARCH");
        assert_eq!(
            workflow.template.start_node.as_ref().map(|n| n.as_str()),
            Some("RESEARCH")
        );
        // Linear chain: each node depends on exactly the previous one.
        assert!(workflow.template.nodes[0].depends_on.is_empty());
        assert_eq!(workflow.template.nodes[3].depends_on[0].as_str(), "CAPTURE");
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
                .any(|e| e.contains("duplicate back-edge"))
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
