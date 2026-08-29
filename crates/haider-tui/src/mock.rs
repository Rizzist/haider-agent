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
        title: "Allow fs_edit — event_store.rs?".to_owned(),
        body: vec![
            "fs_edit wants to modify crates/haider-store/src/event_store.rs".to_owned(),
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
        origin: "fs_edit".to_owned(),
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
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
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
/// providers (openai ×2, anthropic ×2, gemini, local, huggingface) — the
/// launcher's Accounts meta quotes both counts verbatim.
///
/// B6b documented divergence: the sim seeds the Gemini key under `google`;
/// the adapter that actually landed (B6a) registers as `gemini` in
/// provider.list, so the demo world wears the honest registry id.
pub const SEED_ACCOUNTS: usize = 7;
pub const SEED_ACCOUNT_PROVIDERS: usize = 5;

/// The sim's seed accounts VERBATIM (tui.js:146-154) as `/accounts` rows.
/// `SEED_ACCOUNTS`/`SEED_ACCOUNT_PROVIDERS` above must stay derived from
/// this list — the accounts screen test pins that equality.
#[must_use]
pub fn seed_account_rows() -> Vec<crate::app::AccountRow> {
    use haider_protocol::credential::{AuthMethod, CredentialStatus};
    let row = |alias: &str,
               provider: &str,
               method: AuthMethod,
               identity: &str,
               selected: bool,
               base_url: Option<&str>| crate::app::AccountRow {
        alias: alias.to_owned(),
        provider: provider.to_owned(),
        method,
        identity: identity.to_owned(),
        account_identity: None,
        created_at_ms: None,
        status: CredentialStatus::Ok,
        selected,
        base_url: base_url.map(str::to_owned),
    };
    vec![
        row(
            "work-chatgpt",
            "openai",
            AuthMethod::OAuth,
            "you@work.com · ChatGPT",
            true,
            None,
        ),
        row(
            "billing-key",
            "openai",
            AuthMethod::ApiKey,
            "sk-…a91f",
            false,
            None,
        ),
        row(
            "personal-max",
            "anthropic",
            AuthMethod::OAuth,
            "you@me.com · Claude Max",
            true,
            None,
        ),
        row(
            "ci-key",
            "anthropic",
            AuthMethod::ApiKey,
            "sk-ant-…7c2d",
            false,
            None,
        ),
        row(
            "gemini-key",
            "gemini",
            AuthMethod::ApiKey,
            "AIza…4b1",
            true,
            None,
        ),
        row(
            "vllm-local",
            "local",
            AuthMethod::ApiKey,
            "sk-…local · qwen3-coder",
            true,
            Some("http://127.0.0.1:8000/v1"),
        ),
        row(
            "hf-endpoint",
            "huggingface",
            AuthMethod::ApiKey,
            "hf_…5a1 · llama-3-70b",
            true,
            Some("https://llama-3-70b.endpoints.huggingface.cloud/v1"),
        ),
    ]
}

/// Demo `/providers` summaries — consistent with the demo world's account
/// seed and the sim's MODELS providers (the sim has no providers screen;
/// this is W5d demo scaffolding, not sim parity).
#[must_use]
pub fn seed_provider_summaries() -> Vec<haider_rpc::ProviderSummaryWire> {
    use haider_protocol::credential::AuthMethod;
    use haider_rpc::{ProviderApiFamilyWire, ProviderAvailabilityWire, ProviderSummaryWire};
    let summary = |provider: &str,
                   api_family: ProviderApiFamilyWire,
                   endpoint: Option<&str>,
                   models: &[&str],
                   auth: &[AuthMethod],
                   availability: ProviderAvailabilityWire,
                   reason: Option<&str>,
                   default_model: Option<&str>,
                   enabled: bool| ProviderSummaryWire {
        trust: haider_rpc::ProviderTrustWire::Full,
        provider: provider.to_owned(),
        api_family,
        endpoint: endpoint.map(str::to_owned),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: models.iter().map(|&model| model.to_owned()).collect(),
        model_details: models
            .iter()
            .map(|&model| haider_rpc::ModelDetailWire {
                name: model.to_owned(),
                context_window: None,
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            })
            .collect(),
        inventory_fetched_at_ms: None,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Authoritative,
        auth_methods: auth.to_vec(),
        availability,
        availability_reason: reason.map(str::to_owned),
        default_model: default_model.map(str::to_owned),
        enabled,
    };
    vec![
        summary(
            "openai",
            ProviderApiFamilyWire::OpenAiResponses,
            Some("https://api.openai.com/v1"),
            &["gpt-5.6", "gpt-5.6-codex", "o4-mini"],
            &[AuthMethod::OAuth, AuthMethod::ApiKey],
            ProviderAvailabilityWire::Available,
            None,
            Some("gpt-5.6-codex"),
            true,
        ),
        summary(
            "anthropic",
            ProviderApiFamilyWire::AnthropicMessages,
            Some("https://api.anthropic.com"),
            &["claude-opus-5", "claude-sonnet-5"],
            &[AuthMethod::OAuth, AuthMethod::ApiKey],
            ProviderAvailabilityWire::Available,
            None,
            Some("claude-opus-5"),
            true,
        ),
        // B6a landed the Gemini adapter: the demo registry serves what
        // provider.list serves — a real family, endpoint, and inventory
        // (the pre-B6a seed pinned "google · adapter not installed").
        summary(
            "gemini",
            ProviderApiFamilyWire::GeminiGenerateContent,
            Some("https://generativelanguage.googleapis.com/v1beta"),
            &["gemini-3"],
            &[AuthMethod::ApiKey],
            ProviderAvailabilityWire::Available,
            None,
            Some("gemini-3"),
            true,
        ),
        // B6k landed kimi-oauth as a builtin. Signed out, its catalog is
        // undiscovered — provider.list serves exactly this unavailable row
        // (the reason string is the daemon's own), which keeps the demo's
        // honest-unavailable rendering exercised.
        summary(
            "kimi-oauth",
            ProviderApiFamilyWire::OpenAiChatCompletions,
            Some("https://api.kimi.com/coding/v1"),
            &[],
            &[AuthMethod::OAuth],
            ProviderAvailabilityWire::Unavailable,
            Some("provider model inventory is unavailable"),
            None,
            true,
        ),
        summary(
            "local",
            ProviderApiFamilyWire::OpenAiChatCompletions,
            Some("http://127.0.0.1:8000/v1"),
            &["qwen3-coder"],
            &[AuthMethod::ApiKey],
            ProviderAvailabilityWire::Available,
            None,
            Some("qwen3-coder"),
            true,
        ),
        summary(
            "huggingface",
            ProviderApiFamilyWire::OpenAiChatCompletions,
            Some("https://llama-3-70b.endpoints.huggingface.cloud/v1"),
            &["llama-3-70b"],
            &[AuthMethod::ApiKey],
            ProviderAvailabilityWire::Available,
            None,
            Some("llama-3-70b"),
            true,
        ),
        // DeepSeek is a named fixed-origin builtin. The documented aliases
        // are fallback rows only; no context/effort/speed facts are guessed.
        summary(
            "deepseek",
            ProviderApiFamilyWire::OpenAiChatCompletions,
            Some("https://api.deepseek.com"),
            &["deepseek-chat", "deepseek-reasoner"],
            &[AuthMethod::ApiKey],
            ProviderAvailabilityWire::Unavailable,
            Some("provider has no credential"),
            Some("deepseek-chat"),
            true,
        ),
        summary(
            "xai",
            ProviderApiFamilyWire::OpenAiChatCompletions,
            Some("https://api.x.ai/v1"),
            &["grok-4.6", "grok-4.5", "grok-4.3", "grok-build-0.1"],
            &[AuthMethod::ApiKey],
            ProviderAvailabilityWire::Unavailable,
            Some("provider has no credential"),
            Some("grok-4.6"),
            true,
        ),
        summary(
            "grok-oauth",
            ProviderApiFamilyWire::OpenAiChatCompletions,
            Some("https://cli-chat-proxy.grok.com/v1"),
            &["grok-4.6", "grok-4.5"],
            &[AuthMethod::OAuth],
            ProviderAvailabilityWire::Unavailable,
            Some("provider has no credential"),
            Some("grok-4.6"),
            true,
        ),
    ]
}

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
            device: "local",
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
            device: "this-mac",
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
            name: "fs_edit",
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
            name: "fs_edit",
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
        ros: Some(name.ros),
        callsign: name.callsign,
        hon: name.hon,
        full: name.full,
        name: "web-index".to_owned(),
        model: "gpt-5.6".to_owned(),
        device: "this-mac".to_owned(),
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

/// Materialize the three seed sessions as full [`SessionState`]s (sim
/// seeds, tui.js:497-579) — stable ids `demo-session-1..3`, roster heads
/// 0-2, seed transcripts applied verbatim, the L1 seed's live `web-index`
/// chip attached, and the token meter Usage-seeded. `turns_offset` keeps
/// each seed's advertised turn count while real turns still move the
/// number.
///
/// `first_generation` is where the caller's monotonic allocator stands;
/// the three rows take it and the next two. IDENTITY NEVER RECURS (W3c3.1,
/// review P1-5): `/reset` reseeds this same world, and hardcoding
/// generations 1-3 let a REPLACEMENT surface wear a dead one — defeating,
/// at the only site that matters, the very law `next_ui_generation`'s
/// monotonicity exists to keep.
///
/// The SESSION IDS stay `demo-session-1..3` regardless. They are the
/// demo's stable persistence identities — what the v1→v2 upcaster maps a
/// legacy numeric `id: 2` onto — and a session id is not a generation
/// (report R11 cut 1).
#[must_use]
pub fn seed_session_states(first_generation: u64) -> Vec<crate::session::SessionState> {
    sample_sessions()
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let ordinal = u64::try_from(index).unwrap_or(0);
            // The stable demo id (the old numeric ids verbatim; `+ 1`
            // skips `UiGeneration::SCRATCH`) and a FRESH generation.
            let id =
                crate::identity::demo_session_id(crate::identity::UiGeneration::new(ordinal + 1));
            let ui_gen =
                crate::identity::UiGeneration::new(first_generation.saturating_add(ordinal));
            let mut entry = crate::session::SessionState::neutral(id, ui_gen);
            entry.name = Some(sample.name.to_owned());
            entry.title = Some(sample.blurb.to_owned());
            entry.head = (sample.head.to_owned(), sample.honorific.to_owned());
            entry.head_ros = Some(u64::try_from(index).unwrap_or(0));
            entry.dir = sample.dir.to_owned();
            entry.model_short = sample.model.to_owned();
            entry.device = sample.device.to_owned();
            entry.ago = sample.ago.to_owned();
            entry.branches_offset = sample.branches;
            for row in sample_seed(index) {
                entry.projection.apply_seed_row(row);
            }
            entry
                .projection
                .apply(&haider_protocol::EventPayload::Usage(
                    haider_protocol::provider::Usage {
                        input: sample.tokens,
                        output: 0,
                        reasoning: 0,
                        cached: 0,
                        source: haider_protocol::provider::UsageSource::Estimated,
                        account: None,
                        accounts: Vec::new(),
                        normalized: None,
                        scope: None,
                        cache_cost: None,
                        request: None,
                    },
                ));
            if let Some(seed) = sample_seed_chip(index) {
                entry.chips.push(crate::app::ChipModel::from_seed(seed));
            }
            entry.turns_offset = sample
                .turns
                .saturating_sub(entry.projection.user_row_count());
            entry
        })
        .collect()
}
