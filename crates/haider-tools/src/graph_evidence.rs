//! Explicit M1 testimony from the session model to the daemon graph authority.
//!
//! This tool never inspects or advances a graph itself. The daemon validates
//! the named node against the current open obligation, stamps the graph-wide
//! attempt epoch and fingerprint, and derives every follow-up gate fact.

use crate::{ToolError, ToolResult};
use haider_protocol::graph::{EvidenceVerdict, GRAPH_EVIDENCE_DETAIL_MAX_BYTES, GraphNodeName};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEvidence {
    pub node: GraphNodeName,
    pub verdict: EvidenceVerdict,
    pub detail: String,
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
        if request.detail.len() > GRAPH_EVIDENCE_DETAIL_MAX_BYTES.saturating_mul(4) {
            return Err(ToolError::invalid_argument(format!(
                "graph_evidence detail exceeds the {} byte input limit",
                GRAPH_EVIDENCE_DETAIL_MAX_BYTES.saturating_mul(4)
            )));
        }
        Ok(request)
    }
}

#[must_use]
pub fn graph_evidence_manifest() -> ToolManifest {
    ToolManifest {
        name: "graph_evidence".into(),
        description: "Record bounded green or red evidence for the CURRENT open Convergence Graph obligation. The daemon validates the node, stamps the current attempt epoch and fingerprint, and alone decides whether the gate advances. Child reports are not harvested automatically in M1: read them, then testify here."
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
                    "maxLength": GRAPH_EVIDENCE_DETAIL_MAX_BYTES * 4,
                    "description": "Bounded evidence summary; normalized and fingerprinted by the daemon"
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
