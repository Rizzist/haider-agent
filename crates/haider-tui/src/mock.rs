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

/// The boot checklist shown by the demo — the sim's texts VERBATIM
/// (tui.js:3165-3170): `--demo` IS the sim script, so it plays the sim's
/// story (owner ruling, TUI3a item 5).
pub const DEMO_CHECKS: [&str; 4] = [
    "store open · journal replayed",
    "provider handshake — anthropic ✓",
    "hooks loaded — 1 trusted · 0 pending",
    "worker warm · mesh probe done",
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
        body: vec![
            "fs_patch wants to modify crates/haider-store/src/event_store.rs".to_owned(),
            "effect class: workspace write · reversible via /tree".to_owned(),
        ],
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
#[derive(Debug, Clone, Copy)]
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
    /// The session's own run state is busy (sim `runStates[s.id] !== IDLE`).
    pub running: bool,
    /// Live subagent chips this session owns (sim `sessionLive`,
    /// tui.js:789-792). The L1 seed owns the running `web-index` chip
    /// (tui.js:556-572), which is what makes it the launcher's one live row
    /// — the Rust port used to mark `cellular-pool-fix` busy instead
    /// (review P2-8). `sessionBusy` = `live > 0 || running`.
    pub live_subagents: usize,
    /// The session's working dir (sim `session.dir`) — the header shows it
    /// and `cd` retargets it from there.
    pub dir: &'static str,
}

/// The sim's seeded credential list (tui.js:146-154): 7 accounts across 5
/// providers (openai ×2, anthropic ×2, google, local, huggingface) — the
/// launcher's Accounts meta quotes both counts verbatim.
pub const SEED_ACCOUNTS: usize = 7;
pub const SEED_ACCOUNT_PROVIDERS: usize = 5;
/// The sim's seeded peer list minus the `shell` rung (tui.js:169-174):
/// this-mac + workstation (peer) and hetzner-1 (attached) host sessions;
/// ci-runner-7 is exec-only. The launcher's Peers meta quotes this count.
pub const SEED_HOST_CAPABLE_PEERS: usize = 3;

impl SampleSession {
    /// Sim `sessionBusy` (tui.js:789-792): live subagents OR a busy run.
    #[must_use]
    pub const fn busy(&self) -> bool {
        self.live_subagents > 0 || self.running
    }
}

/// The sim's seed sessions, verbatim identity (owner default roster).
#[must_use]
pub fn sample_sessions() -> Vec<SampleSession> {
    vec![
        SampleSession {
            name: "billing-service",
            dir: "~/dev/diffforge/cloud",
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
            live_subagents: 0,
        },
        SampleSession {
            name: "cellular-pool-fix",
            dir: "~/dev/diffforge/cellular",
            head: "Fatima",
            honorific: "(a)",
            blurb: "Deploy-drain pool orphan hunt",
            branches: 1,
            turns: 4,
            tokens: 41_000,
            model: "gpt-5.6",
            device: "hetzner-1",
            ago: "2h",
            running: false,
            live_subagents: 0,
        },
        SampleSession {
            name: "l1-remote-projects",
            dir: "~/dev/diffforge/web",
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
            live_subagents: 1,
        },
    ]
}

// The old `response_script` placeholder turn is gone: user-typed input now
// runs the sim-verbatim respond() router (`crate::script::respond_beats`).

/// One seeded transcript row of a sample session (sim `U`/`A`/`T`/`N`
/// helpers, tui.js:469-472). Attaching a session REPLAYS these into its
/// projection — it does not start a turn (sim `openSession`, tui.js:1606).
#[derive(Debug, Clone, Copy)]
pub enum SeedRow {
    User(&'static str),
    Agent(&'static str),
    Tool {
        name: &'static str,
        desc: &'static str,
        meta: &'static str,
    },
    Note(&'static str),
}

/// The sim's seeded transcripts, verbatim (tui.js:474-581). Indexed like
/// [`sample_sessions`].
#[must_use]
pub fn sample_seed(index: usize) -> &'static [SeedRow] {
    use SeedRow::{Agent, Note, Tool, User};
    const BILLING: &[SeedRow] = &[
        User("wire stripe webhooks into the billing service and backfill the missing invoices"),
        Agent(
            "Reading the billing module first — the webhook surface lives in cloud/src/billing/. I'll add the endpoint, verify signatures, then replay the missed events.",
        ),
        Tool {
            name: "fs_search",
            desc: "\"invoice.paid\" cloud/src/billing/**",
            meta: "14 matches",
        },
        Tool {
            name: "fs_patch",
            desc: "cloud/src/billing/webhooks.rs",
            meta: "+128 −12",
        },
        Tool {
            name: "process_exec",
            desc: "cargo test -p billing",
            meta: "34 passed",
        },
        Agent(
            "Webhook endpoint is in with signature verification and idempotent event handling. Backfill replays events since the last cursor against the new handler — 212 invoices restored.",
        ),
        Note("◇ checkpoint 7 committed"),
        User("now add retry with idempotency keys on the outbound charge calls"),
        Agent(
            "Wrapping the charge client in a retry budget keyed by an idempotency header — replays can never double-charge.",
        ),
        Tool {
            name: "fs_patch",
            desc: "cloud/src/billing/charge.rs",
            meta: "+64 −9",
        },
        Tool {
            name: "process_exec",
            desc: "cargo test -p billing retry",
            meta: "9 passed",
        },
    ];
    const CELLULAR: &[SeedRow] = &[
        User("find why the cellular DB pool orphans connections after a deploy drain"),
        Agent(
            "Tracing the drain path. The pool guard is dropped before in-flight calls settle, so their connections never return to the pool.",
        ),
        Tool {
            name: "fs_search",
            desc: "pool.acquire cellular/src",
            meta: "23 matches",
        },
        Tool {
            name: "fs_read",
            desc: "cellular/src/pbx/route.rs",
            meta: "412 lines",
        },
        Agent(
            "Fix direction: hold the guard until the call registry empties, then close. Want me to patch it?",
        ),
    ];
    const L1: &[SeedRow] = &[
        User("summarize the L1 remote-projects contract"),
        Agent(
            "L1 keeps remote project state as turn_diff hunks over a pinned base; the dark theme stays default and the light theme is opt-in per stored toggle.",
        ),
    ];
    match index {
        0 => BILLING,
        1 => CELLULAR,
        _ => L1,
    }
}

/// The sim's seeded chip on `s-l1` (tui.js:556-572): `web-index`, claimed
/// at roster index 15 (`Salman (r)`), still running — this is the launcher's
/// one live row, and its background animation is part of the seed.
#[must_use]
pub fn sample_seed_chip(index: usize) -> Option<crate::script::ChipSeed> {
    if index != 2 {
        return None;
    }
    let name = crate::script::roster_at(15);
    Some(crate::script::ChipSeed {
        agent: "seed-l1-sub".to_owned(),
        parent: None,
        callsign: name.callsign,
        hon: name.hon,
        full: name.full,
        name: "web-index".to_owned(),
        model: "gpt-5.6".to_owned(),
        device: "mac-studio".to_owned(),
        state: crate::script::ChipDisplayState::Running,
        tokens: 2100,
        prefill: vec![
            crate::script::ChipPrefill::Note(
                "· delegated locally — indexing the web workspace".to_owned(),
            ),
            crate::script::ChipPrefill::Agent(
                "Building the remote-projects index; I'll report once the hunks are pinned against the base."
                    .to_owned(),
            ),
            crate::script::ChipPrefill::ToolOk {
                name: "fs_search".to_owned(),
                desc: "turn_diff hunks web/src/**".to_owned(),
                meta: "312 matches".to_owned(),
            },
        ],
    })
}

/// A unique item id for a seeded transcript row.
#[must_use]
pub fn seed_item_id(seq: u64) -> haider_protocol::ids::ItemId {
    haider_protocol::ids::ItemId::new(format!("seed-{seq}"))
}
