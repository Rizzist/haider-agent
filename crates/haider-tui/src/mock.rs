//! The demo script — a scripted envelope stream that drives every TUI
//! surface before the daemon exists (`haider tui --demo`). One authoritative
//! story: boot checks → launcher-ready → a turn with streaming text, a tool
//! call, a file change, a pinned plan, usage, and a clean finish.
//!
//! The script is pure data (`Vec<EventPayload>`); pacing is the event loop's
//! concern. Keeping it typed means the demo exercises the exact projection
//! path real envelopes take.

use haider_protocol::EventPayload;
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::ids::{ItemId, MenuId};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{AnswerVia, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use haider_protocol::provider::{Usage, UsageSource};
use haider_protocol::state::{HarnessStatus, ReadinessCheck, RunState};

/// The boot checklist shown by the demo (sim parity lines).
pub const DEMO_CHECKS: [&str; 4] = [
    "store open · journal replayed",
    "provider handshake — fake ✓",
    "hooks loaded — 0 trusted · 0 pending",
    "worker warm · profile locked",
];

fn check(name: &str, ok: bool) -> ReadinessCheck {
    ReadinessCheck {
        name: name.to_owned(),
        ok,
        duration_ms: 40,
    }
}

fn starting(done: usize) -> EventPayload {
    EventPayload::HarnessStatus(HarnessStatus::Starting {
        checks: DEMO_CHECKS
            .iter()
            .enumerate()
            .map(|(index, name)| check(name, index < done))
            .collect(),
    })
}

fn item(n: u32) -> ItemId {
    ItemId::new(format!("demo-item-{n}"))
}

fn todos(states: [TodoState; 3]) -> Vec<TodoItem> {
    let texts = [
        "read the failing test",
        "patch the boundary check",
        "re-run the suite",
    ];
    texts
        .iter()
        .zip(states)
        .enumerate()
        .map(|(index, (text, state))| TodoItem {
            id: index as u32,
            text: (*text).to_owned(),
            state,
            dep: None,
        })
        .collect()
}

/// The full demo payload sequence, in commit order.
#[must_use]
pub fn demo_script() -> Vec<EventPayload> {
    use TodoState::{Completed, Listed, Processing};
    let mut script = Vec::new();

    // Boot: the readiness checklist completes one check at a time.
    for done in 0..=DEMO_CHECKS.len() {
        script.push(starting(done));
    }
    script.push(EventPayload::HarnessStatus(HarnessStatus::Ready));

    // The demo turn.
    script.push(EventPayload::UserMessage {
        text: "fix the failing boundary test in haider-store".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    });
    script.push(EventPayload::RunState(RunState::Queued));
    script.push(EventPayload::RunState(RunState::Thinking));

    // A pinned plan appears.
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: item(1),
        item: TurnItem::Plan {
            items: todos([Processing, Listed, Listed]),
        },
    }));

    // Streaming agent text.
    script.push(EventPayload::RunState(RunState::Streaming));
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: item(2),
        item: TurnItem::AgentMessage {
            text: String::new(),
        },
    }));
    for chunk in [
        "Reading the failing test first — ",
        "the boundary check rejects seq 0 ",
        "but the fixture starts at 0.",
    ] {
        script.push(EventPayload::Item(ItemEvent::Delta {
            item_id: item(2),
            delta: ItemDelta::Text {
                text: chunk.to_owned(),
            },
        }));
    }
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: item(2),
        item: TurnItem::AgentMessage {
            text: "Reading the failing test first — the boundary check rejects seq 0 \
                   but the fixture starts at 0."
                .to_owned(),
        },
    }));

    // A tool call runs.
    script.push(EventPayload::RunState(RunState::RunningTool));
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: item(3),
        item: TurnItem::ToolCall {
            call_id: "demo-call-1".to_owned(),
            name: "fs_read".to_owned(),
            args: serde_json::json!({"path": "crates/haider-store/src/event_store.rs"}),
            status: ToolStatus::InProgress,
        },
    }));
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: item(3),
        item: TurnItem::ToolCall {
            call_id: "demo-call-1".to_owned(),
            name: "fs_read".to_owned(),
            args: serde_json::json!({"path": "crates/haider-store/src/event_store.rs"}),
            status: ToolStatus::Completed,
        },
    }));

    // A permission menu replaces the composer (blocking), then the script
    // self-answers so non-interactive runs stay deterministic — an
    // interactive answer beats it and the later duplicate is ignored.
    let menu_id = MenuId::new("demo-menu-1");
    script.push(EventPayload::MenuOpened(Menu {
        id: menu_id.clone(),
        kind: MenuKind::Permission {
            effect_summary: "patch crates/haider-store/src/event_store.rs".to_owned(),
        },
        title: "Allow fs_patch — event_store.rs?".to_owned(),
        body: vec![],
        options: vec![
            MenuOption {
                key: "allow".to_owned(),
                label: "Allow once".to_owned(),
                detail: None,
                decision: None,
            },
            MenuOption {
                key: "deny".to_owned(),
                label: "Deny".to_owned(),
                detail: None,
                decision: None,
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "fs_patch".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }));
    script.push(EventPayload::RunState(RunState::PermissionRequired {
        menu: menu_id.clone(),
    }));
    script.push(EventPayload::MenuAnswered(MenuAnswer {
        menu: menu_id,
        option_key: Some("allow".to_owned()),
        option_index: 0,
        value: None,
        via: AnswerVia::Tui,
    }));
    script.push(EventPayload::RunState(RunState::RunningTool));

    // Plan progresses; the patch lands as a file change.
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: item(1),
        item: TurnItem::Plan {
            items: todos([Completed, Processing, Listed]),
        },
    }));
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: item(4),
        item: TurnItem::FileChange {
            path: "crates/haider-store/src/event_store.rs".to_owned(),
            added: 4,
            removed: 1,
        },
    }));

    // The plan finishes (unpins into the transcript), usage arrives, done.
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: item(1),
        item: TurnItem::Plan {
            items: todos([Completed, Completed, Completed]),
        },
    }));
    script.push(EventPayload::Usage(Usage {
        input: 18_400,
        output: 2_100,
        reasoning: 900,
        cached: 12_000,
        source: UsageSource::ProviderReported,
        account: None,
    }));
    script.push(EventPayload::RunState(RunState::Done));
    script
}
