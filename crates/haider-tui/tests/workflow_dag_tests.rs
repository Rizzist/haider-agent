//! v0.0.938 workflow DAG view: a flow's SHAPE — what runs after what, and
//! crucially what runs CONCURRENTLY — is drawn from the template's real
//! `depends_on` edges, not reconstructed by a reader from a flat list.

#![allow(clippy::expect_used)]

use haider_protocol::graph::{
    GraphExecutorShape, GraphGateKind, GraphNodeName, GraphNodeSpec, GraphTemplateSpec,
};
use haider_tui::render::workflow_dag_lines;
use haider_tui::theme::ThemeKey;

fn node(name: &str, depends_on: &[&str]) -> GraphNodeSpec {
    GraphNodeSpec {
        name: GraphNodeName::new(name).expect("valid node name"),
        gate: GraphGateKind::CommandGreen,
        executor: GraphExecutorShape::Inline,
        max_attempts: 1,
        max_evidence_per_attempt: None,
        depends_on: depends_on
            .iter()
            .map(|dep| GraphNodeName::new(*dep).expect("valid dep"))
            .collect(),
        verify_slots: Vec::new(),
    }
}

fn template(nodes: Vec<GraphNodeSpec>) -> GraphTemplateSpec {
    GraphTemplateSpec {
        name: "fixture".to_owned(),
        version: 1,
        start_node: None,
        nodes,
    }
}

fn rendered(spec: &GraphTemplateSpec) -> String {
    let theme = ThemeKey::Dark.theme();
    workflow_dag_lines(spec, theme)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// MUTATION CHECK (executed): compute the layer as a node's dependency COUNT
/// rather than 1 + max(layer(deps)) and the diamond below collapses — `docs`
/// and `build` stop sharing a layer, or `verify` lands beside them, and the
/// concurrency line disappears.
#[test]
fn a_diamond_lays_out_in_dependency_layers() {
    // plan → {build, docs} → verify: the classic fan-out/fan-in.
    let spec = template(vec![
        node("PLAN", &[]),
        node("BUILD", &["PLAN"]),
        node("DOCS", &["PLAN"]),
        node("VERIFY", &["BUILD", "DOCS"]),
    ]);
    let text = rendered(&spec);

    assert!(text.contains("4 nodes · 3 layers"), "layer count: {text}");
    // build and docs share a layer, so the view SAYS they run together —
    // the fact a flat "← after" list cannot show.
    assert!(
        text.contains("concurrent — these run together"),
        "the fan-out is announced: {text}"
    );
    // Order follows dependency depth, not declaration order.
    let plan = text.find("PLAN").expect("plan drawn");
    let build = text.find("BUILD").expect("build drawn");
    let verify = text.find("VERIFY").expect("verify drawn");
    assert!(plan < build && build < verify, "layered order: {text}");
    // Edges are drawn from the real spec.
    assert!(text.contains("← BUILD + DOCS"), "fan-in edges: {text}");
}

/// A linear chain has one node per layer and never claims concurrency.
#[test]
fn a_linear_chain_never_claims_concurrency() {
    let spec = template(vec![
        node("ONE", &[]),
        node("TWO", &["ONE"]),
        node("THREE", &["TWO"]),
    ]);
    let text = rendered(&spec);
    assert!(text.contains("3 nodes · 3 layers"), "{text}");
    assert!(
        !text.contains("concurrent"),
        "nothing runs together in a chain: {text}"
    );
}

/// A single node is one layer, and the header stays grammatical.
#[test]
fn a_single_node_renders_without_plural_or_connectors() {
    let spec = template(vec![node("ONLY", &[])]);
    let text = rendered(&spec);
    assert!(text.contains("1 node · 1 layer"), "{text}");
    assert!(!text.contains("concurrent"), "{text}");
}

/// A dependency on a node the template does not contain must not silently
/// change the layout or hang the relaxation — it is ignored for layering and
/// still shown as a declared edge.
#[test]
fn an_unknown_dependency_is_ignored_for_layering_but_still_shown() {
    let spec = template(vec![node("SOLO", &["GHOST"])]);
    let text = rendered(&spec);
    assert!(text.contains("1 node · 1 layer"), "{text}");
    assert!(
        text.contains("← GHOST"),
        "the declared edge is not hidden: {text}"
    );
}
