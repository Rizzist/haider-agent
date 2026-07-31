//! W6b — the live chip wiring deltas (research checklist items 4 + 5):
//! the W6a manifest's persisted task label names the chip, and the
//! DURABLE local-child wait badge carries the tree's count.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::ids::{AgentId, LeaseId};
use haider_protocol::state::{RunState, WaitReason};
use haider_tui::app::ChipModel;
use haider_tui::script::ChipDisplayState;

mod common;
use common::launcher_model;

fn manifest(agent: &str, task: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: task.to_owned(),
        callsign: Some("Ammar".to_owned()),
        model_profile: "fable-5".to_owned(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new("lease-1"),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: None,
    }
}

/// MUTATION CHECK (W6b): leave `ChipModel::from_manifest`'s `name` empty
/// (the pre-W6a shape). Expected runtime failure: the chip below has no
/// label although the manifest persisted one.
#[test]
fn the_manifest_task_labels_the_chip() {
    let chip = ChipModel::from_manifest(&manifest("agent-1", "audit the toolset"));
    assert_eq!(chip.name, "audit the toolset");
}

/// MUTATION CHECK (W6b): count the tree only over the literal `IDLE`
/// badge (the pre-W6a overlay). Expected runtime failure: the DURABLE
/// `Waiting(LocalChild)` badge below stays uncounted — "subagent" with
/// no number — although one live chip is working.
#[test]
fn the_durable_local_child_wait_badge_counts_the_tree() {
    let mut model = launcher_model();
    let mut chip = ChipModel::from_manifest(&manifest("agent-1", "audit the toolset"));
    chip.state = ChipDisplayState::Thinking;
    model.chips.push(chip);
    model
        .projection
        .apply(&EventPayload::RunState(RunState::Waiting {
            reason: WaitReason::LocalChild,
        }));
    let (badge, _) = model.status_badge();
    assert_eq!(badge, "◔ WAITING · 1 subagent");
}
