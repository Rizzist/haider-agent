//! The CLI's JSON is a HAND-MAINTAINED MIRROR of the wire types, and a mirror
//! of a growing struct is a permanent source of silent omissions.
//!
//! Three additive wire fields have already vanished this way — `effort`/`fast`,
//! the `needs_input` coordinates, and (in v0.0.938) `run_id` +
//! `worker_generation`. Each shipped, each was pinned daemon-side, and each was
//! invisible to `haider sessions --json` with no error anywhere. The last one
//! was only noticed because someone went looking AFTER the release.
//!
//! This guard fails when a wire struct gains a field the projection does not
//! name, so the omission is caught at the commit that introduces it rather than
//! by a user who cannot find their data.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/<crate>")
        .to_path_buf()
}

/// Field names declared on a `pub struct <name>` in a source file.
fn wire_fields(source: &str, struct_name: &str) -> Vec<String> {
    let anchor = format!("pub struct {struct_name} {{");
    let Some(start) = source.find(&anchor) else {
        panic!("{struct_name} not found — did it get renamed?");
    };
    let body = &source[start + anchor.len()..];
    let end = body.find("\n}").expect("struct body terminates");
    body[..end]
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("pub "))
        .filter_map(|line| {
            line.trim_start_matches("pub ")
                .split(':')
                .next()
                .map(|name| name.trim().to_owned())
        })
        .collect()
}

/// A rename between two surfaces of the same field is a tripwire: the wire
/// calls it `session_id`, the CLI historically called it `id`, and a consumer
/// reading both silently fails to join the rows. A web-client bridge lost
/// `run_id` for hours partly to this shape. Both names are emitted, and they
/// must stay equal — an alias that drifts is worse than no alias.
///
/// MUTATION CHECK (executed): emit `session_id` from a different source than
/// `id`. Expected RUNTIME failure: the equality assertion below.
#[test]
fn the_session_id_alias_is_emitted_and_never_drifts_from_id() {
    let root = workspace_root();
    let observe = std::fs::read_to_string(root.join("crates/haider-cli/src/observe.rs"))
        .expect("observe.rs readable");
    assert!(
        observe.contains("\"session_id\": self.id"),
        "the alias must be emitted FROM `self.id`, so the two names cannot \
         drift apart — an alias sourced separately is a second field wearing \
         the same name"
    );
    assert!(
        observe.contains("\"id\": self.id"),
        "the historical `id` key stays for existing readers"
    );
}

/// MUTATION CHECK (executed): delete the `"run_id"` emission from
/// `SessionSummaryView::json`. Expected RUNTIME failure: this guard names
/// `run_id` as unprojected — which is exactly the v0.0.938 bug it exists to
/// prevent recurring.
#[test]
fn every_session_summary_wire_field_is_projected_or_deliberately_skipped() {
    let root = workspace_root();
    let frame =
        fs::read_to_string(root.join("crates/haider-rpc/src/frame.rs")).expect("frame.rs readable");
    let observe = fs::read_to_string(root.join("crates/haider-cli/src/observe.rs"))
        .expect("observe.rs readable");

    // Fields the CLI deliberately does not surface, each with a REASON. Adding
    // a name here is a decision to omit it; leaving it out is an oversight.
    // The distinction is the entire point of the list.
    let deliberately_skipped: &[(&str, &str)] = &[
        ("head_seq", "internal cursor, not operator-facing"),
        ("session_id", "emitted under BOTH `id` and `session_id`"),
        ("metadata", "flattened into provider/model/workspace"),
        ("title", "projected directly"),
        ("run_state", "projected via run_state_name"),
        ("turn_count", "not surfaced by the sessions view"),
        ("footprint_tokens", "flattened into `footprint`"),
        ("footprint_truth", "flattened into `footprint`"),
        ("workspace_cwd", "not surfaced by the sessions view"),
        (
            "agent_metrics",
            "flattened into `cache`; fleet carries a DIFFERENT snapshot (roots/rollup) and never had usage",
        ),
        ("parent_session_id", "lineage is its own view"),
        ("kind", "lineage is its own view"),
        ("account_alias", "accounts view surfaces this"),
    ];

    // These wire names are promoted or renamed in the CLI, so merely finding
    // the output key is insufficient: `provider` already existed through
    // nested digest metadata and let the new top-level field pass this guard
    // without being read. Require the roster source expression itself.
    let source_sensitive_projection: &[(&str, &str)] = &[
        ("provider", "&summary.provider"),
        ("last_model", "&summary.last_model"),
    ];

    let wire = wire_fields(&frame, "SessionSummary");
    assert!(
        wire.len() > 10,
        "parser found only {} fields — it has broken, not the projection",
        wire.len()
    );

    let missing: Vec<&String> = wire
        .iter()
        .filter(|field| {
            !deliberately_skipped
                .iter()
                .any(|(name, _)| *name == field.as_str())
        })
        .filter(|field| {
            source_sensitive_projection
                .iter()
                .find(|(name, _)| *name == field.as_str())
                .map_or_else(
                    || !observe.contains(&format!("\"{field}\"")),
                    |(_, source)| !observe.contains(source),
                )
        })
        .collect();

    assert!(
        missing.is_empty(),
        "SessionSummary gained wire field(s) the CLI projection never emits, so \
         they are invisible to `haider sessions --json` with no error anywhere: \
         {missing:?}\n\nEither project them, or add them to \
         `deliberately_skipped` WITH a reason."
    );
}
