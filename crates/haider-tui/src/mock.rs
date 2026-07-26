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

fn turn_item(turn: u64, n: u32) -> ItemId {
    ItemId::new(format!("t{turn}-item-{n}"))
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

/// Boot beats only: readiness checks then Ready (drives boot → launcher).
#[must_use]
pub fn boot_script() -> Vec<EventPayload> {
    let mut script = Vec::new();
    for done in 0..=DEMO_CHECKS.len() {
        script.push(starting(done));
    }
    script.push(EventPayload::HarnessStatus(HarnessStatus::Ready));
    script
}

/// The full demo payload sequence, in commit order (plain/CI path).
#[must_use]
pub fn demo_script() -> Vec<EventPayload> {
    let mut script = boot_script();
    script.extend(turn_script(0));
    script
}

/// The classic scripted session turn (attach/auto-play path).
#[must_use]
pub fn turn_script(turn: u64) -> Vec<EventPayload> {
    use TodoState::{Completed, Listed, Processing};
    // The demo turn.
    let mut script = vec![
        EventPayload::UserMessage {
            text: "fix the failing boundary test in haider-store".to_owned(),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        },
        EventPayload::RunState(RunState::Queued),
        EventPayload::RunState(RunState::Thinking),
    ];

    // A pinned plan appears.
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: turn_item(turn, 1),
        item: TurnItem::Plan {
            items: todos([Processing, Listed, Listed]),
        },
    }));

    // Streaming agent text.
    script.push(EventPayload::RunState(RunState::Streaming));
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: turn_item(turn, 2),
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
            item_id: turn_item(turn, 2),
            delta: ItemDelta::Text {
                text: chunk.to_owned(),
            },
        }));
    }
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: turn_item(turn, 2),
        item: TurnItem::AgentMessage {
            text: "Reading the failing test first — the boundary check rejects seq 0 \
                   but the fixture starts at 0."
                .to_owned(),
        },
    }));

    // A tool call runs.
    script.push(EventPayload::RunState(RunState::RunningTool));
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: turn_item(turn, 3),
        item: TurnItem::ToolCall {
            call_id: "demo-call-1".to_owned(),
            name: "fs_read".to_owned(),
            args: serde_json::json!({"path": "crates/haider-store/src/event_store.rs"}),
            status: ToolStatus::InProgress,
        },
    }));
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: turn_item(turn, 3),
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
    let menu_id = MenuId::new(format!("t{turn}-menu-1"));
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
        item_id: turn_item(turn, 1),
        item: TurnItem::Plan {
            items: todos([Completed, Processing, Listed]),
        },
    }));
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: turn_item(turn, 4),
        item: TurnItem::FileChange {
            path: "crates/haider-store/src/event_store.rs".to_owned(),
            added: 4,
            removed: 1,
        },
    }));

    // The plan finishes (unpins into the transcript), usage arrives, done.
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: turn_item(turn, 1),
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

/// A launcher sample session row (sim seed parity — display data only until
/// real sessions arrive with the daemon).
#[derive(Debug, Clone)]
pub struct SampleSession {
    pub name: &'static str,
    pub head: &'static str,
    pub honorific: &'static str,
    pub blurb: &'static str,
    pub branches: u32,
    pub turns: u32,
    pub tokens: u64,
    pub model: &'static str,
    pub device: &'static str,
    pub ago: &'static str,
    pub running: bool,
}

/// The sim's seed sessions, verbatim identity (owner default roster).
#[must_use]
pub fn sample_sessions() -> Vec<SampleSession> {
    vec![
        SampleSession {
            name: "billing-service",
            head: "Muhammad",
            honorific: "ﷺ",
            blurb: "Stripe webhooks + invoice backfill",
            branches: 2,
            turns: 2,
            tokens: 118_000,
            model: "fable-5",
            device: "this-mac",
            ago: "12m",
            running: false,
        },
        SampleSession {
            name: "cellular-pool-fix",
            head: "Fatima",
            honorific: "(a)",
            blurb: "Deploy-drain pool orphan hunt",
            branches: 1,
            turns: 4,
            tokens: 41_000,
            model: "gpt-5.6",
            device: "hetzner-1",
            ago: "2h",
            running: true,
        },
        SampleSession {
            name: "l1-remote-projects",
            head: "Ali",
            honorific: "(a)",
            blurb: "L1 remote-projects contract read",
            branches: 1,
            turns: 6,
            tokens: 22_000,
            model: "gemini-3",
            device: "mac-studio",
            ago: "1d",
            running: false,
        },
    ]
}

/// A canned response turn for user-typed demo input: the same envelope shapes
/// the real harness emits, themed around the typed text.
#[must_use]
pub fn response_script(user_text: &str, turn: u64) -> Vec<EventPayload> {
    let mut script = vec![
        EventPayload::UserMessage {
            text: user_text.to_owned(),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        },
        EventPayload::RunState(RunState::Queued),
        EventPayload::RunState(RunState::Thinking),
        EventPayload::RunState(RunState::Streaming),
    ];
    let reply_id = ItemId::new(format!("t{turn}-reply"));
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: reply_id.clone(),
        item: TurnItem::AgentMessage {
            text: String::new(),
        },
    }));
    let head: String = user_text.chars().take(42).collect();
    for chunk in [
        "On it — reading the relevant code before ".to_owned(),
        "changing anything. Task noted: ".to_owned(),
        format!("“{head}”."),
    ] {
        script.push(EventPayload::Item(ItemEvent::Delta {
            item_id: reply_id.clone(),
            delta: ItemDelta::Text { text: chunk },
        }));
    }
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: reply_id.clone(),
        item: TurnItem::AgentMessage {
            text: format!(
                "On it — reading the relevant code before changing anything. \
                 Task noted: “{head}”."
            ),
        },
    }));
    script.push(EventPayload::RunState(RunState::RunningTool));
    let search_id = ItemId::new(format!("t{turn}-search"));
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: search_id.clone(),
        item: TurnItem::ToolCall {
            call_id: "demo-search".to_owned(),
            name: "fs_search".to_owned(),
            args: serde_json::json!({"query": head, "glob": "src/**"}),
            status: ToolStatus::InProgress,
        },
    }));
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: search_id,
        item: TurnItem::ToolCall {
            call_id: "demo-search".to_owned(),
            name: "fs_search".to_owned(),
            args: serde_json::json!({"query": head, "glob": "src/**"}),
            status: ToolStatus::Completed,
        },
    }));
    script.push(EventPayload::RunState(RunState::Streaming));
    let close_id = ItemId::new(format!("t{turn}-close"));
    script.push(EventPayload::Item(ItemEvent::Started {
        item_id: close_id.clone(),
        item: TurnItem::AgentMessage {
            text: String::new(),
        },
    }));
    script.push(EventPayload::Item(ItemEvent::Delta {
        item_id: close_id.clone(),
        delta: ItemDelta::Text {
            text: "That's the demo loop — the real turn engine ships with the daemon wave."
                .to_owned(),
        },
    }));
    script.push(EventPayload::Item(ItemEvent::Completed {
        item_id: close_id,
        item: TurnItem::AgentMessage {
            text: "That's the demo loop — the real turn engine ships with the daemon wave."
                .to_owned(),
        },
    }));
    script.push(EventPayload::Usage(Usage {
        input: 6_200,
        output: 480,
        reasoning: 120,
        cached: 2_400,
        source: UsageSource::ProviderReported,
        account: None,
    }));
    script.push(EventPayload::RunState(RunState::Done));
    script
}
