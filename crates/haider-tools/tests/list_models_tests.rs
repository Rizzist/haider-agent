#![allow(clippy::expect_used)]

use haider_protocol::tool::DispatchMode;
use haider_tools::{LIST_MODELS_FILTER_MAX_BYTES, ListModels, list_models_manifest};

#[test]
fn list_models_manifest_is_an_effectless_cached_catalog_read() {
    let manifest = list_models_manifest();
    assert_eq!(manifest.name, "list_models");
    assert_eq!(manifest.dispatch, DispatchMode::Await);
    assert!(manifest.effects.is_empty());
    assert_eq!(
        manifest.description,
        "List the daemon's already-discovered model catalog. This is a cached local read and never refreshes provider inventory. Use filter when the bounded result is truncated."
    );
    assert_eq!(
        manifest.input_schema["properties"]["filter"]["description"],
        "Optional case-insensitive model/provider/alias substring"
    );
}

#[test]
fn list_models_filter_is_trimmed_optional_and_bounded() {
    assert_eq!(
        ListModels::from_tool_args(serde_json::json!({"filter": "  GLM  "}))
            .expect("valid filter")
            .filter
            .as_deref(),
        Some("GLM")
    );
    assert_eq!(
        ListModels::from_tool_args(serde_json::json!({"filter": "  "}))
            .expect("blank filter is absent")
            .filter,
        None
    );
    assert!(
        ListModels::from_tool_args(
            serde_json::json!({"filter": "x".repeat(LIST_MODELS_FILTER_MAX_BYTES + 1)})
        )
        .is_err()
    );
}
