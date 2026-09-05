//! Explicit M1 testimony from the session model to the daemon graph authority.
//!
//! This tool never inspects or advances a graph itself. The daemon validates
//! the named node against the current open obligation, stamps the graph-wide
//! attempt epoch and fingerprint, and derives every follow-up gate fact.

use crate::{ToolError, ToolResult};
use haider_protocol::graph::{
    EvidenceVerdict, GRAPH_EVIDENCE_DETAIL_MAX_BYTES, GraphNodeName, ProcessSignalRef,
    WorkspaceMutationRef,
};
use haider_protocol::ids::GraphId;
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const GRAPH_EVIDENCE_INPUT_MAX_BYTES: usize = GRAPH_EVIDENCE_DETAIL_MAX_BYTES * 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_id: Option<GraphId>,
    pub node: GraphNodeName,
    pub verdict: EvidenceVerdict,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<ProcessSignalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation: Option<WorkspaceMutationRef>,
    /// Select existing daemon evidence without copying durable receipt IDs
    /// into model context. This is a lookup, not an authority declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_from: Option<String>,
}

impl GraphEvidence {
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
        if args.as_object().is_some_and(|object| {
            [
                "source",
                "authority",
                "image",
                "workspace_revision",
                "computer_observation",
            ]
            .iter()
            .any(|reserved| object.contains_key(*reserved))
        }) {
            return Err(ToolError::invalid_argument(
                "graph_evidence cannot supply daemon-owned evidence provenance",
            ));
        }
        let request: Self = serde_json::from_value(args).map_err(|error| {
            ToolError::invalid_argument(format!("invalid graph_evidence arguments: {error}"))
        })?;
        if request.detail.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "graph_evidence detail must not be empty",
            ));
        }
        // The daemon performs Unicode-safe normalization and bounding. Keep a
        // generous input ceiling here so pathological payloads never reach it.
        if request.detail.len() > GRAPH_EVIDENCE_INPUT_MAX_BYTES {
            return Err(ToolError::invalid_argument(format!(
                "graph_evidence detail exceeds the {} byte input limit",
                GRAPH_EVIDENCE_INPUT_MAX_BYTES
            )));
        }
        if request
            .slot
            .as_deref()
            .is_some_and(|slot| slot.trim().is_empty() || slot.len() > 64)
        {
            return Err(ToolError::invalid_argument(
                "graph_evidence slot must contain 1..=64 UTF-8 bytes",
            ));
        }
        if request
            .subject_digest
            .as_deref()
            .is_some_and(|subject| subject.trim().is_empty() || subject.len() > 128)
        {
            return Err(ToolError::invalid_argument(
                "graph_evidence subject_digest must contain 1..=128 UTF-8 bytes",
            ));
        }
        if request.signal.as_ref().is_some_and(|signal| {
            signal.run_id.as_str().trim().is_empty()
                || signal.call_id.trim().is_empty()
                || signal.effect_id.as_str().trim().is_empty()
        }) {
            return Err(ToolError::invalid_argument(
                "graph_evidence signal coordinates must not be empty",
            ));
        }
        if request.workspace_mutation.as_ref().is_some_and(|mutation| {
            mutation.run_id.as_str().trim().is_empty()
                || mutation.effect_id.as_str().trim().is_empty()
        }) {
            return Err(ToolError::invalid_argument(
                "graph_evidence workspace_mutation coordinates must not be empty",
            ));
        }
        if request.signal.is_some() && request.workspace_mutation.is_some() {
            return Err(ToolError::invalid_argument(
                "graph_evidence accepts either signal or workspace_mutation provenance, not both",
            ));
        }
        if let Some(selector) = request.evidence_from.as_deref() {
            if !matches!(
                selector,
                "latest_process" | "latest_mutation" | "latest_subject"
            ) {
                return Err(ToolError::invalid_argument(
                    "graph_evidence evidence_from must be latest_process, latest_mutation, or latest_subject",
                ));
            }
            if request.signal.is_some()
                || request.workspace_mutation.is_some()
                || request.subject_digest.is_some()
            {
                return Err(ToolError::invalid_argument(
                    "graph_evidence evidence_from cannot be combined with explicit provenance or subject_digest",
                ));
            }
        }
        Ok(request)
    }
}

#[must_use]
pub fn graph_evidence_manifest() -> ToolManifest {
    ToolManifest {
        name: "graph_evidence".into(),
        description: "Record green or red evidence for the current open graph obligation. Use evidence_from=latest_process or latest_mutation for daemon-verified slots, latest_subject for model testimony. The daemon resolves this run's durable provenance and checks freshness and authority before advancing the gate."
            .into(),
        effects: vec![],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "node": {
                    "type": "string",
                    "pattern": "^[A-Z][A-Z0-9_-]{0,63}$",
                    "description": "Must equal one of the current open obligations"
                },
                "graph_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Active graph id from GraphBrief; binds evidence across graph.switch races"
                },
                "verdict": {
                    "type": "string",
                    "enum": ["green", "red"]
                },
                "detail": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": GRAPH_EVIDENCE_INPUT_MAX_BYTES,
                    "description": "Bounded evidence summary; normalized and fingerprinted by the daemon"
                },
                "slot": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "description": "Required when the pinned all-of-N node declares evidence slots"
                },
                "evidence_from": {
                    "type": "string",
                    "enum": ["latest_process", "latest_mutation", "latest_subject"],
                    "description": "Resolve the latest terminal process or applied mutation in this run from the journal; latest_subject supplies only the subject for model testimony. Cannot combine with explicit references. All freshness and authority checks still apply."
                },
                "subject_digest": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 128,
                    "description": "Subject digest emitted by the referenced daemon signal, or the attested subject"
                },
                "signal": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string", "minLength": 1 },
                        "call_id": { "type": "string", "minLength": 1 },
                        "effect_id": { "type": "string", "minLength": 1 }
                    },
                    "required": ["run_id", "call_id", "effect_id"],
                    "additionalProperties": false,
                    "description": "Required for daemon-verified slots; must reference a durable process signal"
                },
                "workspace_mutation": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string", "minLength": 1 },
                        "effect_id": { "type": "string", "minLength": 1 }
                    },
                    "required": ["run_id", "effect_id"],
                    "additionalProperties": false,
                    "description": "Alternative daemon provenance for a durable filesystem mutation"
                }
            },
            "required": ["graph_id", "node", "verdict", "detail"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_parser_accept_a_bounded_general_node() {
        let request = GraphEvidence::from_tool_args(serde_json::json!({
            "node": "VERIFY",
            "verdict": "green",
            "detail": "haider-core passed"
        }))
        .expect("valid evidence");
        assert_eq!(
            request.node,
            GraphNodeName::new("VERIFY").expect("bounded node")
        );
        assert_eq!(request.verdict, EvidenceVerdict::Green);
        assert_eq!(graph_evidence_manifest().name, "graph_evidence");
    }

    #[test]
    fn model_cannot_claim_computer_observation_or_daemon_authority() {
        for (field, value) in [
            (
                "source",
                serde_json::json!({"kind": "computer_observation"}),
            ),
            ("authority", serde_json::json!("daemon_verified")),
            ("image", serde_json::json!({"artifact": "blake3:fake"})),
            (
                "workspace_revision",
                serde_json::json!("workspace-revision:999"),
            ),
            ("computer_observation", serde_json::json!("screenshot")),
        ] {
            let mut args = serde_json::json!({
                "graph_id": "graph",
                "node": "BUILD",
                "verdict": "green",
                "detail": "fabricated screenshot"
            });
            args.as_object_mut()
                .expect("object")
                .insert(field.into(), value);
            assert!(
                GraphEvidence::from_tool_args(args)
                    .expect_err("daemon provenance must be reserved")
                    .to_string()
                    .contains("daemon-owned")
            );
        }
    }

    #[test]
    fn evidence_selector_is_explicit_and_cannot_overwrite_provenance() {
        for selector in ["latest_process", "latest_mutation", "latest_subject"] {
            let args = serde_json::json!({"node":"VERIFY", "verdict":"green", "detail":"checked", "evidence_from":selector});
            assert_eq!(
                GraphEvidence::from_tool_args(args.clone())
                    .expect("selector")
                    .evidence_from
                    .as_deref(),
                Some(selector)
            );
            for (field, value) in [
                ("subject_digest", serde_json::json!("blake3:claimed")),
                (
                    "signal",
                    serde_json::json!({"run_id":"r", "call_id":"c", "effect_id":"e"}),
                ),
                (
                    "workspace_mutation",
                    serde_json::json!({"run_id":"r", "effect_id":"e"}),
                ),
            ] {
                let mut mixed = args.clone();
                mixed[field] = value;
                assert!(GraphEvidence::from_tool_args(mixed).is_err());
            }
        }
        assert!(GraphEvidence::from_tool_args(serde_json::json!({"node":"VERIFY", "verdict":"green", "detail":"checked", "evidence_from":"anything"})).is_err());
    }
}
