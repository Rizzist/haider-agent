//! 972-band-reports golden frames — the read-only report screens on the
//! shared bottom band, pinned text-and-style at the three convention sizes
//! (`tuivirt_common::SIZES`).
//!
//! One golden per report: `/providers`, `/usage`, `/tools`, `/hooks`,
//! `/tree`, the sessions browser, the fleet and the graph. Each pins the
//! redesigned footer — the band's opening rule, the key map in the one-row
//! slot, the closing rule — together with the screen's own body, so a
//! regression in either the report or the shared band shows as a frame
//! diff.
//!
//! Regenerate ONLY deliberately: `UPDATE_TUIVIRT_GOLDENS=1 cargo test -p
//! haider-tui --test band_report_golden_tests`, then review the diff.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::graph::{
    GraphEvidenceTally, GraphExecutorShape, GraphGateKind, GraphNodeName, GraphNodeStatus,
    GraphPhase, GraphStatus, build_node, ship_node, verify_node,
};
use haider_protocol::ids::GraphId;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1, UsageWindowV1,
};
use haider_tui::app::{AppModel, RuntimeMode, Screen};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::script::{ChipDisplayState, ChipSeed};

mod tuivirt_common;
use tuivirt_common::{SIZES, check_golden, draw, launcher_model, session_model};

/// Pin one model at every size.
fn pin(name: &str, model: &AppModel) {
    for (width, height) in SIZES {
        let frame = draw(model, width, height);
        check_golden(name, &frame);
    }
}

/// A live model with the accounts + providers seeds — deterministic
/// identities, no in-flight pulses.
fn seeded_live() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_version = Some("0.0.972".to_owned());
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model.requests.clear();
    model
}

fn chip(agent: &str, name: &str) -> haider_tui::app::ChipModel {
    haider_tui::app::ChipModel::from_seed(ChipSeed {
        agent: agent.to_owned(),
        parent: None,
        ros: None,
        callsign: "Ammar".to_owned(),
        hon: "(r)",
        full: "Ammar ibn Yasir".to_owned(),
        name: name.to_owned(),
        model: "fable-5".to_owned(),
        device: "test-lion-box".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })
}

#[test]
fn providers_report_frames() {
    let mut model = seeded_live();
    model.screen = Screen::Providers;
    pin("report_providers", &model);
}

#[test]
fn usage_report_frames() {
    let mut model = seeded_live();
    model.screen = Screen::Usage;
    model.usage.apply_report(UsageReportV1 {
        generated_at_ms: 1_726_000_000_000,
        accounts: vec![
            AccountUsageReportV1 {
                provider: "anthropic-oauth".to_owned(),
                alias: haider_protocol::ids::CredentialAlias::new("anthropic"),
                identity: Some("max@example.com".to_owned()),
                plan: Some("max_20x".to_owned()),
                auth_method: haider_protocol::credential::AuthMethod::OAuth,
                meter: AccountMeterStateV1::Metered {
                    windows: vec![
                        UsageWindowV1 {
                            window: "five_hour".to_owned(),
                            utilization: 0.83,
                            resets_at_ms: Some(1_726_000_000_000 + 8_070_000),
                            label: None,
                        },
                        UsageWindowV1 {
                            window: "seven_day".to_owned(),
                            utilization: 0.17,
                            resets_at_ms: Some(1_726_000_000_000 + 442_800_999),
                            label: None,
                        },
                    ],
                },
                local: LocalUsageStatsV1::default(),
            },
            AccountUsageReportV1 {
                provider: "openai".to_owned(),
                alias: haider_protocol::ids::CredentialAlias::new("openai"),
                identity: Some("dev@corp.dev".to_owned()),
                plan: None,
                auth_method: haider_protocol::credential::AuthMethod::ApiKey,
                meter: AccountMeterStateV1::LocalOnly,
                local: LocalUsageStatsV1::default(),
            },
        ],
    });
    pin("report_usage", &model);
}

#[test]
fn tools_report_frames() {
    let mut model = session_model();
    model.screen = Screen::Tools;
    model.tools_inventory = Some(haider_protocol::tool::ToolInventorySnapshot {
        tools: vec![
            haider_protocol::tool::ToolInventoryEntry {
                manifest: haider_protocol::tool::ToolManifest {
                    name: "process_exec".into(),
                    description: "run one supervised shell command".into(),
                    effects: vec![haider_protocol::effect::EffectClass::ProcessExec],
                    dispatch: haider_protocol::tool::DispatchMode::Await,
                    input_schema: serde_json::json!({}),
                },
                default: haider_protocol::tool::ToolPermissionDefault::Ask,
            },
            haider_protocol::tool::ToolInventoryEntry {
                manifest: haider_protocol::tool::ToolManifest {
                    name: "fs_read".into(),
                    description: "read one workspace file".into(),
                    effects: vec![haider_protocol::effect::EffectClass::FsRead],
                    dispatch: haider_protocol::tool::DispatchMode::Await,
                    input_schema: serde_json::json!({}),
                },
                default: haider_protocol::tool::ToolPermissionDefault::Allow,
            },
        ],
        remembered_grants: Vec::new(),
    });
    pin("report_tools", &model);
}

#[test]
fn hooks_report_frames() {
    let mut model = session_model();
    model.screen = Screen::Hooks;
    model.hooks.policy = Some("workspace".to_owned());
    model.hooks.rows = Some(vec![
        haider_tui::hooks::HookRow {
            name: "fmt-check".to_owned(),
            digest: "ab".repeat(32),
            source: "hooks.json".to_owned(),
            kind: "exec".to_owned(),
            event: "pre_commit".to_owned(),
            trusted: true,
            trust_state: None,
            decision: false,
            timeout_ms: 5_000,
        },
        haider_tui::hooks::HookRow {
            name: "audit-notify".to_owned(),
            digest: "cd".repeat(32),
            source: "hooks.json".to_owned(),
            kind: "subscribe".to_owned(),
            event: "turn_complete".to_owned(),
            trusted: false,
            trust_state: None,
            decision: true,
            timeout_ms: 5_000,
        },
    ]);
    pin("report_hooks", &model);
}

#[test]
fn tree_report_frames() {
    let mut model = session_model();
    model.screen = Screen::Tree;
    pin("report_tree", &model);
}

#[test]
fn sessions_report_frames() {
    // The demo launcher's seeded roster: fixed titles, dirs and `ago`
    // strings — roster truth with no wall-clock derivation.
    let mut model = launcher_model();
    model.screen = Screen::Sessions;
    pin("report_sessions", &model);
}

#[test]
fn fleet_report_frames() {
    // Demo fleet: the snapshot synthesized from the local chip tree at a
    // fixed clock — the terminal-session shape.
    let mut model = launcher_model();
    model.chips.push(chip("t1-audit", "audit"));
    model.chips.push(chip("t1-docs", "docs"));
    model.open_fleet();
    assert_eq!(model.screen, Screen::Fleet);
    pin("report_fleet", &model);
}

#[test]
fn graph_report_frames() {
    let tally = |green: u32, red: u32, effective_green: u32| GraphEvidenceTally {
        green,
        red,
        effective_green,
        standing_red: u32::from(red > effective_green),
    };
    let node = |name: GraphNodeName,
                attempts_opened: u32,
                current_attempt: Option<u32>,
                evidence: GraphEvidenceTally,
                satisfied: bool| {
        let (gate, executor) = match name.as_str() {
            "BUILD" => (GraphGateKind::CommandGreen, GraphExecutorShape::Inline),
            "VERIFY" => (GraphGateKind::AllOfN { n: 3 }, GraphExecutorShape::FanOut),
            _ => (GraphGateKind::HumanConfirm, GraphExecutorShape::Human),
        };
        GraphNodeStatus {
            node: name,
            gate: Some(gate),
            executor: Some(executor),
            attempts_opened,
            current_attempt,
            evidence,
            evidence_slots: Vec::new(),
            satisfied,
        }
    };
    let mut model = session_model();
    model.screen = Screen::Graph;
    model.graph = Some(GraphStatus {
        graph_id: GraphId::new("g1"),
        template: "ship-loop".into(),
        digest: "abcdef0123456789".into(),
        template_version: 1,
        start_node: Some(build_node()),
        phase: GraphPhase::Active,
        current_node: Some(verify_node()),
        ready_nodes: vec![verify_node()],
        attempt: 2,
        nodes: vec![
            node(build_node(), 2, Some(2), tally(1, 0, 1), true),
            node(verify_node(), 1, Some(2), tally(2, 1, 2), false),
            node(ship_node(), 0, None, tally(0, 0, 0), false),
        ],
        blocked_reason: None,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
    });
    pin("report_graph", &model);
}
