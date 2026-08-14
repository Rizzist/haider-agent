//! Explicit M1 testimony from the session model to the daemon graph authority.
//!
//! This tool never inspects or advances a graph itself. The daemon validates
//! the named node against the current open obligation, stamps the graph-wide
//! attempt epoch and fingerprint, and derives every follow-up gate fact.

use crate::{ToolError, ToolResult};
use haider_protocol::graph::{
    EvidenceVerdict, GRAPH_EVIDENCE_DETAIL_MAX_BYTES, GraphNodeName, ProcessSignalRef,
};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const GRAPH_EVIDENCE_INPUT_MAX_BYTES: usize = GRAPH_EVIDENCE_DETAIL_MAX_BYTES * 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEvidence {
    pub node: GraphNodeName,
    pub verdict: EvidenceVerdict,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<ProcessSignalRef>,
}

impl GraphEvidence {
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
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
        Ok(request)
    }
}

#[must_use]
pub fn graph_evidence_manifest() -> ToolManifest {
    ToolManifest {
        name: "graph_evidence".into(),
        description: "Record bounded green or red evidence for the CURRENT open Convergence Graph obligation. Declared slots replace their own frontier. Daemon-verified slots require a durable process signal reference and matching subject digest; model-attested slots remain explicit testimony. The daemon alone decides whether the gate advances."
            .into(),
        effects: vec![],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "node": {
                    "type": "string",
                    "enum": ["BUILD", "VERIFY", "SHIP"],
                    "description": "Must equal the current open obligation"
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
                }
            },
            "required": ["node", "verdict", "detail"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_parser_share_the_frozen_vocabulary() {
        let request = GraphEvidence::from_tool_args(serde_json::json!({
            "node": "VERIFY",
            "verdict": "green",
            "detail": "haider-core passed"
        }))
        .expect("valid evidence");
        assert_eq!(request.node, GraphNodeName::Verify);
        assert_eq!(request.verdict, EvidenceVerdict::Green);
        assert_eq!(graph_evidence_manifest().name, "graph_evidence");
    }
}
