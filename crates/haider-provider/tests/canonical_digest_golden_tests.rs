#![allow(clippy::expect_used)]
//! turnhygiene pin: the canonical tool-definition digest is a frozen wire law.
//!
//! `streaming_tool_digest_matches_legacy_canonical_dom_bytes` (in
//! `haider-provider/src/lib.rs`) only proves the streaming hasher equals the
//! materialize-then-hash path; if the canonical JSON writer itself changed,
//! both sides would move together. This literal golden freezes the value so
//! a hashing rewrite (canonical JSON straight into BLAKE3) cannot rotate
//! every prompt-cache identity unnoticed.

use haider_provider::{ToolDefinition, canonical_tool_definitions_digest};

const FROZEN_DIGEST: &str = "d22168ee892249ecfc4dfb5beacb3d3ed695d0a7ceb972ffd04a2de8f870018b";

fn frozen_fixture() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "z-tool".into(),
            description: "same".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "z": {"enum": [3, 2, 1]},
                    "a": {"type": "string", "description": "unicode ✓ \"quoted\""}
                },
                "required": ["z"]
            }),
        },
        ToolDefinition {
            name: "a-tool".into(),
            description: "first".into(),
            input_schema: serde_json::json!({"required": ["x"], "type": "object"}),
        },
        ToolDefinition {
            name: "m-tool".into(),
            description: "".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        },
    ]
}

/// MUTATION CHECK: change the canonical JSON writer (key order, escaping,
/// number formatting), the tool sort key, the framing of the hashed array,
/// or the hash function. Expected RUNTIME failure: the digest differs from
/// the frozen literal.
#[test]
fn canonical_tool_definitions_digest_is_a_frozen_wire_law() {
    assert_eq!(
        canonical_tool_definitions_digest(&frozen_fixture()),
        FROZEN_DIGEST
    );
}

/// MUTATION CHECK: hash the definitions in caller order or with the caller's
/// key insertion order. Expected RUNTIME failure: a permuted catalog stops
/// producing the frozen digest, or a semantic change keeps it.
#[test]
fn canonical_tool_definitions_digest_ignores_order_and_tracks_content() {
    let mut permuted = frozen_fixture();
    permuted.reverse();
    permuted[2].input_schema = serde_json::json!({
        "required": ["z"],
        "properties": {
            "a": {"description": "unicode ✓ \"quoted\"", "type": "string"},
            "z": {"enum": [3, 2, 1]}
        },
        "type": "object"
    });
    assert_eq!(canonical_tool_definitions_digest(&permuted), FROZEN_DIGEST);

    let mut reordered_enum = frozen_fixture();
    reordered_enum[0].input_schema["properties"]["z"]["enum"] = serde_json::json!([1, 2, 3]);
    assert_ne!(
        canonical_tool_definitions_digest(&reordered_enum),
        FROZEN_DIGEST,
        "array order is semantic and must change the digest"
    );

    let mut described = frozen_fixture();
    described[2].description = "documented".into();
    assert_ne!(canonical_tool_definitions_digest(&described), FROZEN_DIGEST);
}
