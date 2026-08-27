//! Tool-row preview laws (v0.0.934): size-capped, salient-first, honest
//! absence — cold history rows read like live ones without fattening the
//! pipe.

#![allow(clippy::expect_used)]

use haider_protocol::pipe::{args_preview, result_preview};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};

/// MUTATION CHECK: drop the salient-key preference (serialize whole JSON
/// first) or the 160-scalar cap. Expected runtime failure: the url row
/// below stops leading with the url, or the long row exceeds the cap.
#[test]
fn args_preview_prefers_the_salient_argument_and_stays_bounded() {
    let preview = args_preview(&serde_json::json!({
        "unrelated": "noise",
        "url": "https://generalistai.com/sitemap.xml",
    }))
    .expect("url args preview");
    assert!(
        preview.starts_with("https://generalistai.com/sitemap.xml"),
        "the human-salient argument leads: {preview}"
    );

    let long = "x".repeat(4_000);
    let preview = args_preview(&serde_json::json!({ "cmd": long })).expect("cmd preview");
    assert!(
        preview.chars().count() <= 160,
        "capped at 160 scalars: {}",
        preview.chars().count()
    );

    let multiline = args_preview(&serde_json::json!({
        "pattern": "a\n\n   b\t\tc",
    }))
    .expect("pattern preview");
    assert!(
        !multiline.contains('\n') && !multiline.contains("  "),
        "whitespace normalizes to single spaces: {multiline:?}"
    );
}

/// A result preview reads from the bounded result and is capped; an empty
/// result yields honest absence, never an empty string.
#[test]
fn result_preview_is_bounded_and_absent_when_empty() {
    let result = BoundedResult {
        preview: format!("{}\n tail", "y".repeat(4_000)),
        truncated: true,
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    };
    let preview = result_preview(&result).expect("result preview");
    assert!(preview.chars().count() <= 160);
    assert!(!preview.contains('\n'));

    let empty = BoundedResult {
        preview: String::new(),
        truncated: false,
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    };
    assert_eq!(
        result_preview(&empty),
        None,
        "absence is honest — never an empty preview field"
    );
}
