//! The demo turn engine — a 1:1 port of the sim's `respond()` router
//! (tui.js:1191-1546). Every user-visible string is verbatim (including
//! `·`, `—`, `−`, curly quotes and glyphs); every timing matches the sim's
//! sleeps; the token law is 9/char for user+stream text and +2400 per
//! tool. Scripts are pure data (`Vec<Beat>`); the driver plays them
//! generation-guarded and parks on `AwaitMenu` until the menu's answer
//! selects a continuation arm.

use haider_protocol::EventPayload;
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::ids::{EffectId, ItemId, MenuId};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
use haider_protocol::state::{RunState, WaitReason};

/// One demo event through the driver channel — an envelope, or the demo's
/// own display-only side effects (notes, voice tags, turn-end law).
///
/// `large_enum_variant` allowed deliberately: `Envelope` is the COMMON
/// case (every stream delta, state write and usage frame) — boxing would
/// buy nothing but an allocation per 22 ms token on the hot demo path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum DemoEvent {
    Envelope(EventPayload),
    /// Display-only transcript note (`SessionProjection::push_note`).
    Note(String),
    /// Toggle the voice tag: agent items started while true render
    /// `■ haider · ♪ speaking` (demo-local — the protocol has no voice).
    Voice(bool),
    /// End-of-turn law (sim `finishTurn`, tui.js:1507-1543): queued input
    /// consumes directly (never idle) → auto-compaction check → IDLE.
    TurnEnd,
    /// The ◉ talk hold finished — submit the canned voice phrase.
    TalkFire,
}

/// One script beat (sim beats: state writes, sleeps, streams, tools,
/// notes, menu parks).
///
/// `large_enum_variant` allowed deliberately: `Emit` dominates every
/// script (one per delta token), so boxing the payload would pessimize
/// the common case; beats are transient per-turn data, never retained.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Beat {
    Emit(EventPayload),
    Sleep(u64),
    Note(String),
    /// Token accrual (sim token law). `output` = streamed words bucket.
    Tokens {
        n: u64,
        output: bool,
    },
    /// Reset the token meter to exactly `n` (post-compaction: 6% of window).
    TokensReset(u64),
    Voice(bool),
    /// Park until this menu is answered; the answer's `option_index`
    /// selects the continuation arm (extra indexes clamp to the last arm).
    AwaitMenu {
        menu: MenuId,
        arms: Vec<Vec<Beat>>,
    },
    TurnEnd,
}

/// Sim timing constants (tui.js — see the per-beat tables in the spec).
pub const THINK_MS: u64 = 750;
pub const STREAM_MS: u64 = 22;
pub const AUTO_TITLE_MS: u64 = 1500;
pub const ERRORED_HOLD_MS: u64 = 1800;
pub const DEFERRED_CALLBACK_MS: u64 = 2600;
pub const RATE_RESET_MS: u64 = 3000;
pub const COMPACT_AUTO_MS: u64 = 1400;
pub const COMPACT_MANUAL_MS: u64 = 1200;
pub const TALK_HOLD_MS: u64 = 1300;
/// The ◉ talk canned phrase (tui.js:2050).
pub const TALK_PHRASE: &str = "walk me through the harness entrypoints";

/// GENERIC_INTROS / GENERIC_OUTROS (tui.js:616-625, verbatim).
pub const GENERIC_INTROS: [&str; 3] = [
    "On it. Scanning the workspace for the modules this touches.",
    "Understood — reading the relevant code before changing anything.",
    "Taking this in three steps: locate, patch, verify.",
];
pub const GENERIC_OUTROS: [&str; 3] = [
    "Done — checks are green. Fork this node from /tree if you want to try another direction.",
    "That's in. The diff is small and the tests pass; say the word to extend it.",
    "Finished and verified. Context meter updated — /compact any time it runs hot.",
];

/// The sim ROSTER (tui.js:344-399), verbatim — 38 names drawn strictly in
/// order (callsign · honorific · full). Dignity rule: callsigns are
/// display-only; the wire keys by opaque `AgentId`.
pub const ROSTER: [(&str, &str, &str); 38] = [
    ("Muhammad", "ﷺ", "The Holy Prophet Muhammad"),
    ("Fatima", "(a)", "Fatima al-Zahra"),
    ("Ali", "(a)", "Imam Ali ibn Abi Talib"),
    ("Hasan", "(a)", "Imam Hasan al-Mujtaba"),
    ("Husayn", "(a)", "Imam Husayn"),
    ("Sajjad", "(a)", "Imam Ali Zayn al-Abidin"),
    ("Baqir", "(a)", "Imam Muhammad al-Baqir"),
    ("Sadiq", "(a)", "Imam Ja'far al-Sadiq"),
    ("Kadhim", "(a)", "Imam Musa al-Kadhim"),
    ("Rida", "(a)", "Imam Ali al-Rida"),
    ("Jawad", "(a)", "Imam Muhammad al-Jawad"),
    ("Hadi", "(a)", "Imam Ali al-Hadi"),
    ("Askari", "(a)", "Imam Hasan al-Askari"),
    ("Mahdi", "(aj)", "Imam Muhammad al-Mahdi"),
    ("Abu Dharr", "(r)", "Abu Dharr al-Ghifari"),
    ("Salman", "(r)", "Salman al-Farsi"),
    ("Miqdad", "(r)", "Miqdad ibn al-Aswad"),
    ("Ammar", "(r)", "Ammar ibn Yasir"),
    ("Uwais", "(r)", "Uwais al-Qarani"),
    ("Maytham", "(r)", "Maytham al-Tammar"),
    ("Mukhtar", "(r)", "Mukhtar al-Thaqafi"),
    ("Musa", "(a)", "Prophet Musa"),
    ("Isa", "(a)", "Prophet Isa"),
    ("Yahya", "(a)", "Prophet Yahya"),
    ("Yusuf", "(a)", "Prophet Yusuf"),
    ("Yunus", "(a)", "Prophet Yunus"),
    ("Ibrahim", "(a)", "Prophet Ibrahim"),
    ("Ismail", "(a)", "Prophet Ismail"),
    ("Ishaq", "(a)", "Prophet Ishaq"),
    ("Yaqub", "(a)", "Prophet Yaqub"),
    ("Nuh", "(a)", "Prophet Nuh"),
    ("Adam", "(a)", "Prophet Adam"),
    ("Idris", "(a)", "Prophet Idris"),
    ("Sulayman", "(a)", "Prophet Sulayman"),
    ("Dawud", "(a)", "Prophet Dawud"),
    ("Ayyub", "(a)", "Prophet Ayyub"),
    ("Zakariya", "(a)", "Prophet Zakariya"),
    ("Harun", "(a)", "Prophet Harun"),
];

/// Generation suffixes once the roster wraps (tui.js:399).
pub const ROMAN: [&str; 8] = ["", "II", "III", "IV", "V", "VI", "VII", "VIII"];

/// One claimed roster name (sim `rosterAt`, tui.js:881-886).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterName {
    pub callsign: String,
    pub hon: &'static str,
    pub full: String,
}

impl RosterName {
    /// `cs(n)` — the sim's `"${callsign} ${hon}"` display form.
    #[must_use]
    pub fn cs(&self) -> String {
        format!("{} {}", self.callsign, self.hon)
    }
}

/// `rosterAt(i)`: wraps with a generation suffix applied to callsign AND
/// full name. Seed session heads claim indices 0-2 and the seed chip
/// claims 15; live claims post-increment from 3 (`rosterRef`, tui.js:681).
#[must_use]
pub fn roster_at(index: u64) -> RosterName {
    #[allow(clippy::cast_possible_truncation)]
    let (name, hon, full) = ROSTER[(index % 38) as usize];
    #[allow(clippy::cast_possible_truncation)]
    let generation = ((index / 38) as usize).min(ROMAN.len() - 1);
    let suffix = ROMAN[generation];
    if suffix.is_empty() {
        RosterName {
            callsign: name.to_owned(),
            hon,
            full: full.to_owned(),
        }
    } else {
        RosterName {
            callsign: format!("{name} {suffix}"),
            hon,
            full: format!("{full} {suffix}"),
        }
    }
}

/// The first live claim index (`rosterRef` starts at 3, tui.js:681).
pub const ROSTER_FIRST_CLAIM: u64 = 3;

/// `claimName()` — reads the roster at the counter, then post-increments.
pub fn claim_name(counter: &mut u64) -> RosterName {
    let name = roster_at(*counter);
    *counter += 1;
    name
}

/// Word-boundary test matching JS `\b` (word chars = [A-Za-z0-9_]).
fn word_match(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let at = start + pos;
        let before_ok = at == 0
            || !haystack[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let after = at + needle.len();
        let after_ok = after >= haystack.len()
            || !haystack[after..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if before_ok && after_ok {
            return true;
        }
        start = at + needle.len();
    }
    false
}

/// `/rate.?limit/` — "rate" + at most one joining char + "limit".
fn rate_limit_match(low: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = low[start..].find("rate") {
        let after = start + pos + 4;
        if low[after..].starts_with("limit") {
            return true;
        }
        let mut chars = low[after..].chars();
        if chars.next().is_some() && low[after..].len() > 1 {
            let skip = low[after..].chars().next().map_or(0, char::len_utf8);
            if low[after + skip..].starts_with("limit") {
                return true;
            }
        }
        start = after;
    }
    false
}

/// First `[Image #N]` token in the raw text, if any.
fn image_token(text: &str) -> Option<String> {
    token_scan(text, "[Image #", "]")
}

/// First `[Pasted N lines]` token + its line count.
fn paste_token(text: &str) -> Option<(String, String)> {
    token_scan(text, "[Pasted ", " lines]").map(|token| {
        let digits: String = token
            .trim_start_matches("[Pasted ")
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        (token, digits)
    })
}

fn token_scan(text: &str, prefix: &str, suffix: &str) -> Option<String> {
    let mut start = 0;
    while let Some(pos) = text[start..].find(prefix) {
        let at = start + pos;
        let body = &text[at + prefix.len()..];
        let digits = body.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && body[digits..].starts_with(suffix) {
            let end = at + prefix.len() + digits + suffix.len();
            return Some(text[at..end].to_owned());
        }
        start = at + prefix.len();
    }
    None
}

/// The script builder: shared beat vocabulary for every branch.
struct B {
    beats: Vec<Beat>,
    turn: u64,
    seq: u32,
}

impl B {
    fn new(turn: u64) -> Self {
        Self {
            beats: Vec::new(),
            turn,
            seq: 0,
        }
    }

    fn id(&mut self, kind: &str) -> ItemId {
        self.seq += 1;
        ItemId::new(format!("t{}-{kind}-{}", self.turn, self.seq))
    }

    fn emit(&mut self, payload: EventPayload) {
        self.beats.push(Beat::Emit(payload));
    }

    fn state(&mut self, run: RunState) {
        self.emit(EventPayload::RunState(run));
    }

    fn sleep(&mut self, ms: u64) {
        self.beats.push(Beat::Sleep(ms));
    }

    fn note(&mut self, text: &str) {
        self.beats.push(Beat::Note(text.to_owned()));
    }

    /// stream(text) — sim tui.js:1229-1242: per word-token (words AND
    /// whitespace runs) append + 9 tok/char + 22 ms.
    fn stream(&mut self, text: &str) {
        let item_id = self.id("msg");
        self.emit(EventPayload::Item(ItemEvent::Started {
            item_id: item_id.clone(),
            item: TurnItem::AgentMessage {
                text: String::new(),
            },
        }));
        for token in split_word_tokens(text) {
            self.emit(EventPayload::Item(ItemEvent::Delta {
                item_id: item_id.clone(),
                delta: ItemDelta::Text {
                    text: token.clone(),
                },
            }));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = ((token.chars().count() as f64) * 9.0).round() as u64;
            self.beats.push(Beat::Tokens { n, output: true });
            self.sleep(STREAM_MS);
        }
        self.emit(EventPayload::Item(ItemEvent::Completed {
            item_id,
            item: TurnItem::AgentMessage {
                text: text.to_owned(),
            },
        }));
    }

    /// tool(name, desc, dur, meta) — sim tui.js:1243-1252.
    fn tool(&mut self, name: &str, desc: &str, dur: u64, meta: &str, ok: bool) {
        let item_id = self.id("tool");
        self.state(RunState::RunningTool);
        self.emit(EventPayload::Item(ItemEvent::Started {
            item_id: item_id.clone(),
            item: TurnItem::ToolCall {
                call_id: item_id.as_str().to_owned(),
                name: name.to_owned(),
                args: serde_json::json!({ "desc": desc }),
                status: ToolStatus::InProgress,
            },
        }));
        self.sleep(dur);
        self.emit(EventPayload::Item(ItemEvent::Completed {
            item_id: item_id.clone(),
            item: TurnItem::ToolCall {
                call_id: item_id.as_str().to_owned(),
                name: name.to_owned(),
                args: serde_json::json!({ "desc": desc, "meta": meta }),
                status: if ok {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Failed
                },
            },
        }));
        self.beats.push(Beat::Tokens {
            n: 2400,
            output: false,
        });
    }

    /// A tool row pushed already-complete (the deferred callback,
    /// tui.js:1398): no running phase, NO token accrual.
    fn tool_done(&mut self, name: &str, desc: &str, meta: &str) {
        let item_id = self.id("tool");
        self.emit(EventPayload::Item(ItemEvent::Completed {
            item_id: item_id.clone(),
            item: TurnItem::ToolCall {
                call_id: item_id.as_str().to_owned(),
                name: name.to_owned(),
                args: serde_json::json!({ "desc": desc, "meta": meta }),
                status: ToolStatus::Completed,
            },
        }));
    }

    fn streaming(&mut self) {
        self.state(RunState::Streaming);
    }
}

/// Sim `text.split(/(\s+)/)` — words and whitespace runs, both kept.
fn split_word_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_ws: Option<bool> = None;
    for ch in text.chars() {
        let ws = ch.is_whitespace();
        if current_ws != Some(ws) && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        current_ws = Some(ws);
        current.push(ch);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Build the full respond() turn for user text (sim tui.js:1191-1546).
/// `generic_counter` is the sim's `genRef`: the generic branch
/// post-increments; the test branch reads without incrementing.
/// `roster_counter` is the sim's `rosterRef` (claims post-increment).
/// `mode` rides the UserMessage envelope (queued turns carry `Queue`).
#[must_use]
pub fn respond_beats(
    user_text: &str,
    voice: bool,
    mode: haider_protocol::DeliveryMode,
    turn: u64,
    generic_counter: &mut u64,
    roster_counter: &mut u64,
) -> Vec<Beat> {
    let low = user_text.to_lowercase();
    let mut b = B::new(turn);

    if voice {
        b.beats.push(Beat::Voice(true));
    } else {
        // The turn's user row (the reducer already pushed it for voice).
        b.emit(EventPayload::UserMessage {
            text: user_text.to_owned(),
            attachments: vec![],
            mode,
        });
    }
    // Preamble (tui.js:1254-1260): user tokens · THINKING 750 · STREAMING.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let user_tokens = ((user_text.chars().count() as f64) * 9.0).round() as u64;
    b.beats.push(Beat::Tokens {
        n: user_tokens,
        output: false,
    });
    b.state(RunState::Thinking);
    b.sleep(THINK_MS);
    b.streaming();

    if low.contains("subagent") {
        branch_subagent(&mut b, &low, roster_counter);
    } else if ["crash", "unstable", "unreliable", "corrupt"]
        .iter()
        .any(|k| low.contains(k))
    {
        branch_crash(&mut b);
    } else if word_match(&low, "prod")
        || low.contains("deploy")
        || low.contains("migrate")
        || word_match(&low, "push")
    {
        branch_prod(&mut b);
    } else if ["auth", "deleg", "split", "machin", "device"]
        .iter()
        .any(|k| low.contains(k))
    {
        branch_auth(&mut b, roster_counter);
    } else if [
        "custom tool",
        "preview env",
        "deploy the preview",
        "dispatch mode",
    ]
    .iter()
    .any(|k| low.contains(k))
    {
        branch_custom_tool(&mut b);
    } else if ["test", "flake", "fail"].iter().any(|k| low.contains(k)) || word_match(&low, "ci") {
        branch_test(&mut b, *generic_counter);
    } else if rate_limit_match(&low) || word_match(&low, "429") || low.contains("quota") {
        branch_rate_limit(&mut b);
    } else if low.contains("plan todo") {
        branch_plan_todo(&mut b);
    } else {
        let index = *generic_counter;
        *generic_counter += 1;
        branch_generic(&mut b, user_text, &low, index);
    }

    if voice {
        b.beats.push(Beat::Voice(false));
    }
    b.beats.push(Beat::TurnEnd);
    b.beats
}

/// §1.1 `/subagent/` — commit 1 slice: the parent-side beats verbatim
/// (names claimed from the roster exactly as the sim does). The live chips
/// themselves land with the subagent system (commit 2); until then a
/// closing note says so honestly.
fn branch_subagent(b: &mut B, low: &str, roster_counter: &mut u64) {
    let plural = word_match(low, "subagents");
    let tests_name = claim_name(roster_counter);
    if plural {
        let docs_name = claim_name(roster_counter);
        b.stream(&format!(
            "Spinning up two subagents — {} on the tests, {} on the docs. They run concurrently; click a chip under the input to watch one live.",
            tests_name.cs(),
            docs_name.cs(),
        ));
        b.tool(
            "agent_spawn",
            &format!("{} · tests → local · gpt-5.6", tests_name.callsign),
            800,
            "spawned · lease ok",
            true,
        );
        b.tool(
            "agent_spawn",
            &format!("{} · docs → local · gemini-3", docs_name.callsign),
            700,
            "spawned · lease ok",
            true,
        );
    } else {
        b.stream(&format!(
            "Spinning up a subagent — {} takes the test suite. It runs concurrently; click its chip under the input to watch it live.",
            tests_name.cs(),
        ));
        b.tool(
            "agent_spawn",
            &format!("{} · tests → local · gpt-5.6", tests_name.callsign),
            800,
            "spawned · lease ok",
            true,
        );
    }
    b.streaming();
    b.stream(
        "While they work I'm wiring the harness entrypoints here. The tests subagent will pause on a question — its chip flips to an amber ? when it does.",
    );
    b.tool("fs_patch", "src/core/harness.rs", 900, "+24 −6", true);
    b.streaming();
    b.stream(
        "My side is done. Chip glyphs: ○ idle · ◐ running · ⚒ tool · ? input required · ✓ done.",
    );
    b.note("· live subagent chips land with the next wave — the parent turn above is the sim's exact script");
}

/// §1.2 `/crash|unstable|unreliable|corrupt/` (tui.js:1291-1318).
fn branch_crash(b: &mut B) {
    b.stream(
        "Reproducing the failure path with the real job — if it dies mid-write, we reconcile from the effect journal instead of guessing.",
    );
    b.tool(
        "process_exec",
        "cargo run --bin migrate -- --batch 7",
        1500,
        "exit 137 · connection lost mid-write",
        false,
    );
    b.state(RunState::EffectOutcomeUnknown);
    b.note("· effect_outcome_unknown — the write may or may not have committed");
    let menu_id = MenuId::new(format!("t{}-recovery", b.turn));
    b.emit(EventPayload::MenuOpened(Menu {
        id: menu_id.clone(),
        kind: MenuKind::Recovery {
            effect: EffectId::new("e-4411"),
        },
        title: "recovery — process_exec outcome unknown".to_owned(),
        body: vec![
            "cargo run --bin migrate -- --batch 7 died mid-write (exit 137)".to_owned(),
            "effect class: externally transactional · idempotency key present".to_owned(),
        ],
        options: vec![
            option("probe", "probe & reconcile from the journal (recommended)"),
            option("retry", "retry from checkpoint ◇7"),
            option("errored", "mark run errored — stop here"),
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "process_exec".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }));

    // Shared tail for probe/retry.
    let tail = |probe_first: bool| -> Vec<Beat> {
        let mut arm = B::new(b.turn);
        arm.seq = 90 + u32::from(probe_first);
        if probe_first {
            arm.state(RunState::RunningTool);
            arm.tool(
                "fs_read",
                "journal: effect e-4411 · idempotency key check",
                900,
                "not committed ✓ safe to retry",
                true,
            );
        }
        arm.tool(
            "process_exec",
            "cargo run --bin migrate -- --batch 7",
            1400,
            "42 rows migrated",
            true,
        );
        arm.streaming();
        arm.stream(
            "Reconciled and retried — the journal proved the first attempt never committed, so no double-write was possible.",
        );
        arm.beats.push(Beat::TurnEnd);
        arm.beats
    };
    let mut errored = B::new(b.turn);
    errored.seq = 95;
    errored.note("· run → errored · terminal state is honest — nothing was retried");
    errored.state(RunState::Errored);
    errored.sleep(ERRORED_HOLD_MS);
    // Protocol law says terminal states never change; the demo emits Done
    // 1.8 s later as script license to restore the idle badge (documented
    // divergence — the sim returns to IDLE the same way).
    errored.state(RunState::Done);
    b.beats.push(Beat::AwaitMenu {
        menu: menu_id,
        arms: vec![tail(true), tail(false), errored.beats],
    });
}

/// §1.3 `/\bprod\b|deploy|migrate|\bpush\b/` (tui.js:1319-1344).
fn branch_prod(b: &mut B) {
    b.stream("This touches production — the tool call needs your approval before anything runs.");
    let menu_id = MenuId::new(format!("t{}-permission", b.turn));
    b.state(RunState::PermissionRequired {
        menu: menu_id.clone(),
    });
    b.emit(EventPayload::MenuOpened(Menu {
        id: menu_id.clone(),
        kind: MenuKind::Permission {
            effect_summary: "cargo run --bin migrate -- --prod".to_owned(),
        },
        title: "process_exec requests approval".to_owned(),
        body: vec![
            "cargo run --bin migrate -- --prod".to_owned(),
            "effect class: externally transactional · db writes".to_owned(),
            "an \"always\" answer creates rule: process_exec(cargo run --bin migrate:*)".to_owned(),
        ],
        options: vec![
            option("once", "allow once"),
            option("session", "allow for this session — adds the rule above"),
            option("deny", "deny — tell the agent why"),
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "process_exec".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }));
    let allow = |with_rule: bool, turn: u64| -> Vec<Beat> {
        let mut arm = B::new(turn);
        arm.seq = 90 + u32::from(with_rule);
        if with_rule {
            arm.note("· session rule added: process_exec(cargo run --bin migrate:*)");
        }
        arm.state(RunState::RunningTool);
        arm.tool(
            "process_exec",
            "cargo run --bin migrate -- --prod",
            1500,
            "42 rows migrated",
            true,
        );
        arm.streaming();
        arm.stream("Migration applied cleanly — journaled under its idempotency key.");
        arm.beats.push(Beat::TurnEnd);
        arm.beats
    };
    let mut deny = B::new(b.turn);
    deny.seq = 95;
    deny.note("· denied with reason — the model is told, not just blocked");
    deny.streaming();
    deny.stream(
        "Understood — leaving production untouched. I'll stage the migration as a reviewed patch instead.",
    );
    deny.beats.push(Beat::TurnEnd);
    b.beats.push(Beat::AwaitMenu {
        menu: menu_id,
        arms: vec![allow(false, b.turn), allow(true, b.turn), deny.beats],
    });
}

/// §1.4 `/auth|deleg|split|machin|device/` — commit 1 slice: parent beats
/// verbatim; the hetzner-1 chip itself lands with the subagent system.
fn branch_auth(b: &mut B, roster_counter: &mut u64) {
    let auth_name = claim_name(roster_counter);
    b.stream(&format!(
        "Splitting this: {} {} takes the service core on hetzner-1 while I wire the local side here.",
        auth_name.callsign, auth_name.hon,
    ));
    b.tool(
        "agent_spawn",
        &format!("{} · auth-svc → hetzner-1 · gpt-5.6", auth_name.callsign),
        900,
        "lease accepted · epoch 3",
        true,
    );
    b.tool("fs_patch", "web/src/lib/session.ts", 800, "+41 −7", true);
    b.tool(
        "process_exec",
        "pnpm test --filter web",
        1100,
        "18 passed",
        true,
    );
    b.streaming();
    b.stream(
        "Local wiring is done and verified. The delegated agent is patching the service core; its result lands as a fenced patch you accept from the chip below.",
    );
    b.sleep(900);
    b.tool(
        "agent_control",
        "collect auth-svc result",
        700,
        "patch accepted ✓",
        true,
    );
    b.streaming();
    b.stream("Delegation collected — one winning result, integrated with conflict check. Both sides green.");
}

/// §1.5 custom tools — 3 dispatch modes incl. the deferred WAITING park.
fn branch_custom_tool(b: &mut B) {
    b.stream(
        "Three custom tools are registered for this repo — I'll use all three dispatch modes so you can see how each one treats its result.",
    );
    b.tool(
        "notify_slack",
        "#eng · 'preview build starting'",
        450,
        "dispatched · fire-and-forget · no result awaited",
        true,
    );
    b.note("· dispatch = fire-and-forget — the turn continued the instant it was sent");
    b.streaming();
    b.stream("Now the deploy itself — this one I await, because the next step needs the env id.");
    b.tool(
        "preview_deploy",
        "svc/auth → ephemeral env",
        1300,
        "await · exit 0 · env pv-5521",
        true,
    );
    b.note("· dispatch = await — blocked in TOOL_RUNNING until the env id came back");
    b.streaming();
    b.stream(
        "The smoke suite is slow, so it runs deferred: the tool hands back a ticket now and calls back when it finishes. I stay reachable meanwhile.",
    );
    b.tool(
        "preview_smoke",
        "smoke suite → pv-5521",
        700,
        "accepted · ticket ct-91 · result arrives async",
        true,
    );
    b.note("· dispatch = deferred — parking in WAITING(dependency) on ct-91 · still messageable");
    b.state(RunState::Waiting {
        reason: WaitReason::Other {
            tag: "dependency · custom tool ct-91".to_owned(),
        },
    });
    b.sleep(DEFERRED_CALLBACK_MS);
    b.tool_done(
        "preview_smoke",
        "◇ ct-91 → tool_result",
        "smoke green · 18 passed",
    );
    b.note("· callback resolved ct-91 — the deferred tool woke the turn back up");
    b.streaming();
    b.stream(
        "All three landed: a fire-and-forget notice, an awaited deploy, and a deferred smoke run that called back green. Preview is live at pv-5521.",
    );
}

/// §1.6 `/test|flake|fail|ci\b/` — reads the generic counter WITHOUT
/// incrementing (tui.js:1411).
fn branch_test(b: &mut B, generic_counter: u64) {
    b.stream("Running the suite first to get a clean failure signature.");
    b.tool(
        "process_exec",
        "cargo test --workspace",
        1300,
        "2 failed · 214 passed",
        false,
    );
    b.streaming();
    b.stream(
        "Two failures, same root: the fixture clock is frozen before the lease renewal path. Patching the fixture, not the code.",
    );
    b.tool("fs_patch", "tests/fixtures/clock.rs", 700, "+9 −3", true);
    b.tool(
        "process_exec",
        "cargo test --workspace",
        1200,
        "216 passed",
        true,
    );
    b.streaming();
    #[allow(clippy::cast_possible_truncation)]
    b.stream(GENERIC_OUTROS[(generic_counter % 3) as usize]);
}

/// §1.7 rate limit — rotation, exhausted menu, WAITING auto-resume.
fn branch_rate_limit(b: &mut B) {
    b.stream("Kicking the heavy sweep off — this will lean on the provider hard.");
    b.tool(
        "fs_search",
        "usages src/** (bulk sweep)",
        700,
        "1,204 matches",
        true,
    );
    b.note("· 5h limit on openai/work-chatgpt (Codex oauth) — rotating, oauth preferred");
    b.note("· account → openai/billing-key (api key) · mid-session, like a model change");
    b.sleep(900);
    b.note("· weekly cap now hit on BOTH openai accounts — 5h waits won't help, weekly is spent");
    let menu_id = MenuId::new(format!("t{}-exhausted", b.turn));
    b.emit(EventPayload::MenuOpened(Menu {
        id: menu_id.clone(),
        kind: MenuKind::Exhausted,
        title: "openai accounts weekly-capped — rate limited".to_owned(),
        body: vec![
            "work-chatgpt (Codex oauth)   weekly: 0 left · natural reset Mon 00:00 · manual reset available".to_owned(),
            "billing-key (api)            weekly: 0 left · natural reset Mon 00:00".to_owned(),
            "reset lever is worth spending: work-chatgpt is fully spent, so a reset forfeits nothing".to_owned(),
        ],
        options: vec![
            option("burn", "burn the Codex weekly reset on work-chatgpt — resume now"),
            option("wait", "wait for the natural reset — auto-resume"),
            option("stop", "stop this run"),
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "accounts".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }));
    let tail = |turn: u64, seq: u32| -> Vec<Beat> {
        let mut arm = B::new(turn);
        arm.seq = seq;
        arm.state(RunState::RunningTool);
        arm.tool(
            "fs_search",
            "usages src/** (resumed)",
            900,
            "1,204 matches",
            true,
        );
        arm.streaming();
        arm.stream("Resumed exactly where the limiter cut us off — the sweep is done.");
        arm.beats.push(Beat::TurnEnd);
        arm.beats
    };
    let mut burn = B::new(b.turn);
    burn.seq = 80;
    burn.note(
        "· Codex weekly reset burned on work-chatgpt (fully spent → nothing forfeited) · fresh week restored",
    );
    burn.beats.extend(tail(b.turn, 81));
    let mut wait = B::new(b.turn);
    wait.seq = 85;
    wait.note("· parked until the natural weekly reset — auto-resume armed");
    wait.state(RunState::Waiting {
        reason: WaitReason::Other {
            tag: "weekly reset · openai Mon 00:00".to_owned(),
        },
    });
    wait.sleep(RATE_RESET_MS);
    wait.note("· reset passed — auto-resumed on openai/work-chatgpt (oauth), no human needed");
    wait.beats.extend(tail(b.turn, 86));
    let mut stop = B::new(b.turn);
    stop.seq = 89;
    stop.note("· run stopped — accounts stay weekly-capped until Monday");
    stop.state(RunState::Done);
    b.beats.push(Beat::AwaitMenu {
        menu: menu_id,
        arms: vec![burn.beats, wait.beats, stop.beats],
    });
}

/// §1.8 `/plan todo/` — dep-gated pinned panel → unpin to transcript.
fn branch_plan_todo(b: &mut B) {
    const TEXTS: [&str; 4] = [
        "scope the harness entrypoints",
        "patch the run loop for typed states",
        "wire WAITING propagation through subagents",
        "run the suite and report",
    ];
    let todos = |states: [TodoState; 4]| -> Vec<TodoItem> {
        TEXTS
            .iter()
            .zip(states)
            .enumerate()
            .map(|(index, (text, state))| TodoItem {
                #[allow(clippy::cast_possible_truncation)]
                id: index as u32,
                text: (*text).to_owned(),
                state,
                dep: if index == 0 {
                    None
                } else {
                    #[allow(clippy::cast_possible_truncation)]
                    Some(index as u32 - 1)
                },
            })
            .collect()
    };
    use TodoState::{Completed, Listed, Processing};
    let plan_id = b.id("plan");
    b.emit(EventPayload::Item(ItemEvent::Started {
        item_id: plan_id.clone(),
        item: TurnItem::Plan {
            items: todos([Listed, Listed, Listed, Listed]),
        },
    }));
    b.stream(
        "Planned as four todos — tracking above the input; each unlocks when the one it depends on completes.",
    );
    let work: [(&str, &str, u64, &str); 4] = [
        ("fs_search", "entrypoints src/**", 700, "6 matches"),
        ("fs_patch", "src/core/run_loop.rs", 900, "+42 −9"),
        ("fs_patch", "src/core/waiting.rs", 800, "+18 −3"),
        ("process_exec", "cargo test --workspace", 1200, "218 passed"),
    ];
    let states_at = |i: usize, processing: bool| -> [TodoState; 4] {
        let mut states = [Listed; 4];
        for (j, state) in states.iter_mut().enumerate() {
            *state = if j < i {
                Completed
            } else if j == i && processing {
                Processing
            } else if j == i {
                Completed
            } else {
                Listed
            };
        }
        states
    };
    for (i, (name, desc, dur, meta)) in work.iter().enumerate() {
        b.emit(EventPayload::Item(ItemEvent::Completed {
            item_id: plan_id.clone(),
            item: TurnItem::Plan {
                items: todos(states_at(i, true)),
            },
        }));
        b.tool(name, desc, *dur, meta, true);
        b.emit(EventPayload::Item(ItemEvent::Completed {
            item_id: plan_id.clone(),
            item: TurnItem::Plan {
                items: todos(states_at(i, false)),
            },
        }));
    }
    b.streaming();
    b.stream("All four todos done — the completed plan just unpinned into the transcript.");
}

/// §1.9 default: image-token / paste-token / generic.
fn branch_generic(b: &mut B, user_text: &str, low: &str, index: u64) {
    #[allow(clippy::cast_possible_truncation)]
    let i = (index % 3) as usize;
    if let Some(img) = image_token(user_text) {
        b.stream(
            "Reading the pasted screenshot first — extracting the UI regions before touching code.",
        );
        b.tool(
            "artifact_manage",
            &format!("ingest {img} → CAS"),
            700,
            "img_7f3a · 214 KB",
            true,
        );
    } else if let Some((token, lines)) = paste_token(user_text) {
        b.stream(&format!(
            "Parsing the pasted block ({lines} lines) — treating it as reference, not instructions."
        ));
        b.tool(
            "artifact_manage",
            &format!("ingest {token} → CAS"),
            600,
            "txt_19c2",
            true,
        );
    } else {
        b.stream(GENERIC_INTROS[i]);
    }
    b.streaming();
    let first_two: Vec<&str> = low.split_whitespace().take(2).collect();
    b.tool(
        "fs_search",
        &format!("\"{}\" src/**", first_two.join(" ")),
        600,
        "9 matches",
        true,
    );
    b.tool("fs_read", "src/core/harness.rs", 500, "388 lines", true);
    b.tool("fs_patch", "src/core/harness.rs", 850, "+37 −11", true);
    b.tool("process_exec", "cargo check", 950, "clean", true);
    b.streaming();
    b.stream(GENERIC_OUTROS[i]);
}

fn option(key: &str, label: &str) -> MenuOption {
    MenuOption {
        key: key.to_owned(),
        label: label.to_owned(),
        detail: None,
        decision: None,
    }
}

/// Auto-compaction beats (§5): shared by the 85% auto path (1400 ms) and
/// `/compact` (1200 ms). `finish` re-enters the turn-end law for auto;
/// manual ends at IDLE directly.
#[must_use]
pub fn compaction_beats(before: u64, after: u64, manual: bool) -> Vec<Beat> {
    let mut beats = Vec::new();
    if !manual {
        beats.push(Beat::Note(
            "· context at 85% — compacting (dead branches first, live path last)".to_owned(),
        ));
    }
    beats.push(Beat::Emit(EventPayload::RunState(RunState::Compacting)));
    beats.push(Beat::Sleep(if manual {
        COMPACT_MANUAL_MS
    } else {
        COMPACT_AUTO_MS
    }));
    beats.push(Beat::Emit(EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(format!("compact-{before}")),
        item: TurnItem::ContextCompaction {
            summary_artifact: haider_protocol::ids::ArtifactRef::new("blake3:demo-compact"),
            tokens_before: Some(before),
            tokens_after: Some(after),
        },
    })));
    beats.push(Beat::TokensReset(after));
    if manual {
        beats.push(Beat::Emit(EventPayload::RunState(RunState::Done)));
    } else {
        beats.push(Beat::TurnEnd);
    }
    beats
}

/// The full auto-title note (sim tui.js:1221-1227), fired 1.5 s after the
/// turn starts.
#[must_use]
pub fn title_note(blurb: &str) -> String {
    format!("· session titled — “{blurb}” (background micro-call · never enters the prompt)")
}

/// Wrap a legacy `Vec<EventPayload>` script (the attach replay) as beats
/// with the classic demo pacing.
#[must_use]
pub fn from_legacy(payloads: Vec<EventPayload>) -> Vec<Beat> {
    let mut beats = Vec::new();
    for payload in payloads {
        let pace = crate::runtime::demo_pace(&payload).as_millis();
        beats.push(Beat::Sleep(u64::try_from(pace).unwrap_or(300)));
        beats.push(Beat::Emit(payload));
    }
    beats
}
