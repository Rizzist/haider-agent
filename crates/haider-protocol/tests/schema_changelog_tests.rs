#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::item::TurnItem;
use std::fs;
use std::path::Path;

// Each table generates both the changelog inventory and an exhaustive match.
// A new enum variant therefore cannot be made to compile by adding only a match
// arm: its table entry is automatically checked against the changelog marker.
macro_rules! payload_kinds {
    ($($pattern:pat => $kind:literal),+ $(,)?) => {
        const PAYLOAD_KINDS: &[&str] = &[$($kind),+];

        #[allow(dead_code)]
        fn payload_kind(payload: &EventPayload) -> &'static str {
            match payload {
                $($pattern => $kind),+
            }
        }
    };
}

payload_kinds! {
        EventPayload::HarnessStatus(_) => "harness_status",
        EventPayload::SessionState(_) => "session_state",
        EventPayload::RunState(_) => "run_state",
        EventPayload::RunFailed { .. } => "run_failed",
        EventPayload::ClientDiagnostic { .. } => "client_diagnostic",
        EventPayload::IdleDecayed => "idle_decayed",
        EventPayload::MenuOpened(_) => "menu_opened",
        EventPayload::MenuAnswered(_) => "menu_answered",
        EventPayload::MenuClosed { .. } => "menu_closed",
        EventPayload::UserMessage { .. } => "user_message",
        EventPayload::PeerMessage(_) => "peer.message",
        EventPayload::QueueChanged(_) => "queue_changed",
        EventPayload::Item(_) => "item",
        EventPayload::Effect(_) => "effect",
        EventPayload::ToolResult { .. } => "tool_result",
        EventPayload::NodeCommitted(_) => "node_committed",
        EventPayload::AgentSpawned(_) => "agent_spawned",
        EventPayload::AgentReport(_) => "agent_report",
        EventPayload::AgentChipState { .. } => "agent_chip_state",
        EventPayload::GateReport(_) => "gate_report",
        EventPayload::GraphPinned(_) => "graph_pinned",
        EventPayload::GraphAttemptOpened(_) => "graph_attempt_opened",
        EventPayload::EvidenceRecorded(_) => "evidence_recorded",
        EventPayload::GraphGateSatisfied(_) => "graph_gate_satisfied",
        EventPayload::GraphAdvanced(_) => "graph_advanced",
        EventPayload::GraphNodeReadied(_) => "graph_node_readied",
        EventPayload::GraphBlocked(_) => "graph_blocked",
        EventPayload::GraphCompleted(_) => "graph_completed",
        EventPayload::GraphAbandoned(_) => "graph_abandoned",
        EventPayload::GraphSuperseded(_) => "graph_superseded",
        EventPayload::GraphFinalizationDeferred(_) => "graph_finalization_deferred",
        EventPayload::ProcessSignalRecorded(_) => "process_signal_recorded",
        EventPayload::GraphRunSetOpened(_) => "graph_run_set_opened",
        EventPayload::TodoGraphAttached(_) => "todo_graph_attached",
        EventPayload::ChildGraphAttached(_) => "child_graph_attached",
        EventPayload::ChildTemplateObserved(_) => "child_template_observed",
        EventPayload::ChildTemplatePromoted(_) => "child_template_promoted",
        EventPayload::Rotation(_) => "rotation",
        EventPayload::Usage(_) => "usage",
        EventPayload::CheckpointRecorded(_) => "checkpoint_recorded",
        EventPayload::LockdownRefused(_) => "lockdown.refused",
        EventPayload::LockdownQuota(_) => "lockdown.quota",
        EventPayload::ProviderTrustChanged(_) => "provider.trust_changed",
        EventPayload::ProviderAuthChanged(_) => "provider.auth_changed",
}

macro_rules! item_kinds {
    ($($pattern:pat => $kind:literal),+ $(,)?) => {
        const ITEM_KINDS: &[&str] = &[$($kind),+];

        #[allow(dead_code)]
        fn item_kind(item: &TurnItem) -> &'static str {
            match item {
                $($pattern => $kind),+
            }
        }
    };
}

item_kinds! {
        TurnItem::AgentMessage { .. } => "agent_message",
        TurnItem::IncompleteAgentMessage { .. } => "incomplete_agent_message",
        TurnItem::Reasoning { .. } => "reasoning",
        TurnItem::ToolCall { .. } => "tool_call",
        TurnItem::CommandExecution { .. } => "command_execution",
        TurnItem::FileChange { .. } => "file_change",
        TurnItem::ChildSpawn { .. } => "child_spawn",
        TurnItem::ChildResult { .. } => "child_result",
        TurnItem::Plan { .. } => "plan",
        TurnItem::ContextCompaction { .. } => "context_compaction",
        TurnItem::Extension { .. } => "extension",
        TurnItem::Refusal { .. } => "refusal",
}

fn terminal_kinds() -> Vec<String> {
    let source = include_str!("../../haider-client/src/headless.rs");
    let enum_body = source
        .split_once("pub enum HeadlessTerminalKind {")
        .expect("HeadlessTerminalKind declaration")
        .1
        .split_once('}')
        .expect("HeadlessTerminalKind closing brace")
        .0;
    enum_body
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_suffix(','))
        .map(camel_to_snake)
        .collect()
}

fn camel_to_snake(name: &str) -> String {
    let mut result = String::new();
    for (index, character) in name.chars().enumerate() {
        if character.is_uppercase() {
            if index != 0 {
                result.push('_');
            }
            result.extend(character.to_lowercase());
        } else {
            result.push(character);
        }
    }
    result
}

fn assert_documented(
    changelog: &str,
    family: &str,
    kinds: impl IntoIterator<Item = impl AsRef<str>>,
) {
    for kind in kinds {
        let marker = format!("`{family}:{}`", kind.as_ref());
        assert!(
            changelog.contains(&marker),
            "event schema changelog is missing {marker}"
        );
    }
}

#[test]
fn every_current_automation_kind_is_pinned_in_the_schema_changelog() {
    let changelog_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/event-schema-changelog.md");
    let changelog = fs::read_to_string(&changelog_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", changelog_path.display()));

    assert_documented(&changelog, "payload", PAYLOAD_KINDS);
    assert_documented(&changelog, "item", ITEM_KINDS);
    assert_documented(&changelog, "terminal", terminal_kinds());
}
