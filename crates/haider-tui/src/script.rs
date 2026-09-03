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
    /// The think window closed — the driver picks the branch NOW (sim
    /// tui.js:1259), so the generic/roster counters advance only for turns
    /// that actually survive it (review P2-11).
    Dispatch {
        text: String,
        voice: bool,
        turn: u64,
    },
    /// The 1.5 s auto-title micro-call returned: the driver names the
    /// session and pushes the note TOGETHER, inside the callback (sim
    /// tui.js:1219-1227, review P2-12). `origin` is the surface GENERATION
    /// the call was made for — the sim's callback looks the session up by
    /// id and does nothing if it is gone, and it is NOT cancelled by an
    /// interrupt (review r2 P2-6).
    AutoTitle {
        origin: crate::identity::UiGeneration,
        text: String,
    },
    /// A menu answer from the model's outbox. It rides the never-cancelled
    /// control tag for DELIVERY, but `origin` (the surface GENERATION that
    /// rendered the card) is checked at consumption (review r2 P1-1).
    Answer {
        origin: crate::identity::UiGeneration,
        answer: haider_protocol::menu::MenuAnswer,
    },
    /// The ◉ talk hold finished — submit the canned voice phrase.
    TalkFire,
    // ---- Chip events (§2), tagged with the owning chip's arm ----
    ChipAdd(Box<ChipSeed>),
    ChipState {
        agent: String,
        state: ChipDisplayState,
    },
    ChipEmit {
        agent: String,
        payload: EventPayload,
    },
    ChipNote {
        agent: String,
        text: String,
    },
    ChipTokens {
        agent: String,
        n: u64,
    },
    ChipQuestion {
        agent: String,
        recovery: bool,
        text: String,
        options: Vec<String>,
    },
    ChipResolve {
        agent: String,
        state: ChipDisplayState,
    },
    ChipQuestionClear {
        agent: String,
        state: ChipDisplayState,
    },
    /// Run the close lifecycle (script arm / ✕ both land here).
    ChipCloseReq {
        agent: String,
    },
    /// The 5 s removal timer fired.
    ChipRemove {
        agent: String,
    },
    /// The 120 ms autoResumeParent defer fired — check the §2.7 guards.
    AutoResume,
    // ---- Aura events (§3), tagged with the AURA guard's generation ----
    AuraState(AuraState),
    AuraEmit(EventPayload),
    AuraNote(String),
    AuraVoice(bool),
    AuraRosterPush {
        name: String,
        device: String,
    },
    AuraRosterPatch {
        name: String,
        state: Option<ChipDisplayState>,
        activity: String,
    },
    AuraLog(String),
    /// The aura hold-to-talk fired the canned phrase.
    AuraTalkFire,
}

/// One script beat (sim beats: state writes, sleeps, streams, tools,
/// notes, menu parks, chip and aura operations).
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
    /// Hand the branch choice back to the driver once the think window has
    /// closed (review P2-11 — the sim evaluates `low` AFTER `await
    /// sleep(750)`, tui.js:1254-1260).
    Dispatch {
        text: String,
        voice: bool,
        turn: u64,
    },
    /// Park until this menu is answered; the answer's `option_index`
    /// selects the continuation arm (extra indexes clamp to the last arm).
    AwaitMenu {
        menu: MenuId,
        arms: Vec<Vec<Beat>>,
    },
    TurnEnd,
    // ---- Subagent chips (§2 — demo-local tree; keys are opaque ids) ----
    /// Add a chip to the tree (under `seed.parent` when nested).
    ChipAdd(Box<ChipSeed>),
    /// Set a chip's display state (sim 9-state vocabulary).
    ChipState {
        agent: String,
        state: ChipDisplayState,
    },
    /// Route an ordinary envelope into the CHIP's own projection —
    /// "a child is the same object" (chip streams/tools/user rows/menus).
    ChipEmit {
        agent: String,
        payload: EventPayload,
    },
    /// A note row inside the chip's transcript.
    ChipNote {
        agent: String,
        text: String,
    },
    /// Chip token accrual (+8/char at stream END, +1800/tool — chipOps).
    ChipTokens {
        agent: String,
        n: u64,
    },
    /// Set the chip's pending question (the amber `?` / recovery `⌁`) AND
    /// its state in ONE patch — the sim writes both in a single `mutChip`
    /// (tui.js:925-933 tests, 948-956 docs), and `respondChip`'s
    /// steer-queue gate reads exactly that pair (tui.js:1105): an observer
    /// must never catch `input_required` without its question. `recovery`
    /// selects the state the sim pairs with each card: `error` for the `⌁`
    /// recovery card, `input_required` for the amber `?`.
    ChipQuestion {
        agent: String,
        recovery: bool,
        text: String,
        options: Vec<String>,
    },
    /// Resolve the chip's question AND set its state in one patch (sim
    /// `answerChip`'s single `mutChip`, tui.js:1043-1064).
    ChipResolve {
        agent: String,
        state: ChipDisplayState,
    },
    /// Clear the chip's question AND set its state in one patch (sim
    /// respondChip step 3: `{ state: "thinking", question: null }`,
    /// tui.js:1108).
    ChipQuestionClear {
        agent: String,
        state: ChipDisplayState,
    },
    /// Run the close lifecycle on a chip (flags + 5 s removal + resume).
    ChipClose {
        agent: String,
    },
    /// Play a concurrent child script (childRunTests/Docs, nested child) on
    /// the named chip's OWN arm — the parent beat stream continues
    /// immediately, and closing that chip stops exactly this script.
    ChipScript {
        agent: String,
        beats: Vec<Beat>,
    },
    /// Sim `autoResumeParent` (§2.7): a 120 ms deferred, guard-checked
    /// resume of the parked parent turn.
    AutoResume,
    // ---- Aura (§3 — demo-local orchestrator surface) ----
    AuraState(AuraState),
    /// Route an envelope into the aura transcript (16 ms streams).
    AuraEmit(EventPayload),
    AuraNote(String),
    /// Spoken tag for aura agent rows (`■ aura · ♪`).
    AuraVoice(bool),
    AuraRosterPush {
        name: String,
        device: String,
    },
    AuraRosterPatch {
        name: String,
        state: Option<ChipDisplayState>,
        activity: String,
    },
    AuraLog(String),
}

/// Sim chip display states (tui.js:332-342) — the protocol `ChipState`
/// lacks `Running`, so the demo keeps the sim's 9-state vocabulary
/// display-local (spec §2.8 recommendation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipDisplayState {
    Idle,
    Thinking,
    Streaming,
    Running,
    Tool,
    InputRequired,
    Waiting,
    Done,
    Error,
}

impl ChipDisplayState {
    /// The protocol's `ChipState` → this display vocabulary (W3c3, report
    /// R11 cut 2: `AgentChipState` is the SOLE chip-state authority on the
    /// live stream). `Running` has no protocol twin, so it stays demo-only;
    /// `Closed` maps to `Done` because closing is the `ChipModel::closed`
    /// flag plus the sweep, not a badge — a closed child's last honest
    /// badge is that it finished.
    #[must_use]
    pub const fn from_protocol(state: &haider_protocol::agent::ChipState) -> Self {
        use haider_protocol::agent::ChipState;
        match state {
            ChipState::Idle => Self::Idle,
            ChipState::Thinking => Self::Thinking,
            ChipState::Streaming => Self::Streaming,
            ChipState::Tool => Self::Tool,
            ChipState::Waiting => Self::Waiting,
            ChipState::InputRequired | ChipState::PermissionRequired => Self::InputRequired,
            ChipState::Done | ChipState::Closed => Self::Done,
            ChipState::Error => Self::Error,
        }
    }

    /// True while the chip's turn is ACTIVELY RUNNING — the chip-view twin of
    /// [`crate::projection::SessionProjection::is_turn_active`], and the gate
    /// for that view's `● thinking…` tail. The session and the subagent screen
    /// share one law: the indicator is up for the whole run, not just the
    /// THINKING beat, so the sim/demo surface cannot drift from the real UI.
    ///
    /// The exclusions mirror the session's exactly: `Waiting` (a child holds
    /// the turn, and that case already prints its own "waiting on N child"
    /// tail row — including it here would stack two indicators), `Idle`,
    /// `InputRequired` (blocked on the user, with a menu on screen), and the
    /// terminal `Done`/`Error`. Call it on `display_state()`, never on the raw
    /// `state`, so a chip promoted to `Waiting` by live children is judged by
    /// the same truth its badge shows.
    #[must_use]
    pub const fn is_turn_active(self) -> bool {
        matches!(
            self,
            Self::Thinking | Self::Streaming | Self::Running | Self::Tool
        )
    }

    /// `CHIP_GLYPH` (tui.js:332-342); closed chips render `⊘` everywhere.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Idle => "○",
            Self::Thinking => "●",
            Self::Streaming => "▮",
            Self::Running => "◐",
            Self::Tool => "⚒",
            Self::InputRequired => "?",
            Self::Waiting => "◔",
            Self::Done => "✓",
            Self::Error => "✗",
        }
    }

    /// Uppercase label for the chip-view state badge.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Thinking => "THINKING",
            Self::Streaming => "STREAMING",
            Self::Running => "RUNNING",
            Self::Tool => "TOOL",
            Self::InputRequired => "INPUT_REQUIRED",
            Self::Waiting => "WAITING",
            Self::Done => "DONE",
            Self::Error => "ERROR",
        }
    }
}

/// A chip's seed data (sim chip literal): identity + display fields +
/// pre-filled transcript rows (§1.4's pre-seeded auth chip).
#[derive(Debug, Clone)]
pub struct ChipSeed {
    pub agent: String,
    pub parent: Option<String>,
    /// The roster index this chip's callsign was claimed at (`None` for
    /// chips named outside the roster). Persisted so a reload resumes the
    /// honour-roll past it (sim spreads `rosterAt(i)`'s `ros` into the
    /// chip, tui.js:559; load reads it at 715-721).
    pub ros: Option<u64>,
    pub callsign: String,
    pub hon: &'static str,
    pub full: String,
    pub name: String,
    pub model: String,
    pub device: String,
    pub state: ChipDisplayState,
    pub tokens: u64,
    pub prefill: Vec<ChipPrefill>,
}

/// One pre-filled chip transcript row.
#[derive(Debug, Clone)]
pub enum ChipPrefill {
    Note(String),
    Agent(String),
    ToolOk {
        name: String,
        desc: String,
        meta: String,
    },
}

/// Sim aura orb states (tui.js:121-138).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuraState {
    Idle,
    Listening,
    Thinking,
    Orchestrating,
    Speaking,
}

impl AuraState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Listening => "LISTENING",
            Self::Thinking => "THINKING",
            Self::Orchestrating => "ORCHESTRATING",
            Self::Speaking => "SPEAKING",
        }
    }

    /// Orb stand-in glyph (a terminal cell cannot animate the sim's CSS
    /// orb — approximated per state, documented divergence).
    #[must_use]
    pub const fn orb(self) -> &'static str {
        match self {
            Self::Idle => "◌",
            Self::Listening => "◉",
            Self::Thinking => "●",
            Self::Orchestrating => "◎",
            Self::Speaking => "◍",
        }
    }
}

/// Sim timing constants (tui.js — see the per-beat tables in the spec).
pub const THINK_MS: u64 = 750;
pub const STREAM_MS: u64 = 22;
/// Chip streams pace at 18 ms/word (chipOps, tui.js:898-915).
pub const CHIP_STREAM_MS: u64 = 18;
/// Aura streams pace at 16 ms/word (tui.js:2058).
pub const AURA_STREAM_MS: u64 = 16;
/// The auto-resume stream paces at 20 ms/word (tui.js:960-1013).
pub const RESUME_STREAM_MS: u64 = 20;
/// respondChip's thinking beat (tui.js:1097-1161).
pub const CHIP_THINK_MS: u64 = 650;
/// Closed chips leave the tree after 5 s (tui.js:1168-1185).
pub const CHIP_REMOVE_MS: u64 = 5000;
/// autoResumeParent defers 120 ms before its guards (tui.js:960-1013).
pub const AUTO_RESUME_DEFER_MS: u64 = 120;
/// The aura talk hold (tui.js:2128-2132).
pub const AURA_TALK_MS: u64 = 1100;
/// The aura talk canned phrase, pinned to Haider's local-only placement.
pub(crate) const AURA_TALK_PHRASE: &str = "spin up the auth service locally and run its tests";
pub const AUTO_TITLE_MS: u64 = 1500;
pub const ERRORED_HOLD_MS: u64 = 1800;
pub const DEFERRED_CALLBACK_MS: u64 = 2600;
pub const RATE_RESET_MS: u64 = 3000;
pub const COMPACT_AUTO_MS: u64 = 1400;
pub const COMPACT_MANUAL_MS: u64 = 1200;
/// The sim's transient `IDLE → 30 ms → COMPACTING` window before an
/// auto-compaction (tui.js:1507-1519, review P2-13).
pub const COMPACT_IDLE_GAP_MS: u64 = 30;
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
    /// The claimed roster index (sim `rosterAt` returns `{ ros: i, … }`,
    /// tui.js:398). Recorded on heads and chips so the persistence load can
    /// resume the honour-roll past every claim (TUI4c-13b guard 3).
    pub ros: u64,
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
    let generation = (index / 38) as usize;
    // Sim `ROMAN[gen] || gen + 1` (tui.js:399): past VIII the suffix falls
    // back to the plain generation NUMBER rather than clamping.
    let suffix = ROMAN
        .get(generation)
        .map_or_else(|| (generation + 1).to_string(), |roman| (*roman).to_owned());
    if suffix.is_empty() {
        RosterName {
            ros: index,
            callsign: name.to_owned(),
            hon,
            full: full.to_owned(),
        }
    } else {
        RosterName {
            ros: index,
            callsign: format!("{name} {suffix}"),
            hon,
            full: format!("{full} {suffix}"),
        }
    }
}

/// The first live claim index (`rosterRef` starts at 3, tui.js:681).
pub const ROSTER_FIRST_CLAIM: u64 = 3;

/// `claimName()` — reads the roster at the counter, then post-increments.
/// The counter is SHARED (an `AtomicU64` on the driver) so a callsign is
/// burned exactly when the sim burns it: at branch dispatch, never while a
/// script is merely being built (review P2-11).
pub fn claim_name(counter: &std::sync::atomic::AtomicU64) -> RosterName {
    roster_at(counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
}

/// The sim's token unit: JS `String.length` counts UTF-16 CODE UNITS, so a
/// non-BMP emoji is 2 units (18 tokens at 9/unit), not 1 scalar (review
/// P2-11 — the Rust port used to count Unicode scalars).
fn js_len(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

/// `round(len * rate)` on the sim's UTF-16 length.
fn js_tokens(text: &str, rate: u64) -> u64 {
    js_len(text).saturating_mul(rate)
}

/// True when `haystack[at + needle.len()..]` starts on a JS `\b` boundary
/// (word chars = `[A-Za-z0-9_]`).
fn trailing_boundary(haystack: &str, end: usize) -> bool {
    end >= haystack.len()
        || !haystack[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn leading_boundary(haystack: &str, at: usize) -> bool {
    at == 0
        || !haystack[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// JS `/\bNEEDLE\b/` — boundaries on BOTH sides (`\bprod\b`, `\bpush\b`,
/// `\b429\b`).
fn word_match(haystack: &str, needle: &str) -> bool {
    scan(haystack, needle, true, true)
}

/// JS `/NEEDLE\b/` — a TRAILING boundary only. `pci` therefore matches
/// `ci\b` (the `p` is a word char, so a two-sided match would reject it)
/// while `ascii` does NOT (it ends `ii`, never `ci` on a boundary) — the
/// round-1 review's example was wrong and the round-2 review confirmed the
/// correction.
fn suffix_match(haystack: &str, needle: &str) -> bool {
    scan(haystack, needle, false, true)
}

fn scan(haystack: &str, needle: &str, lead: bool, trail: bool) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let at = start + pos;
        let end = at + needle.len();
        if (!lead || leading_boundary(haystack, at)) && (!trail || trailing_boundary(haystack, end))
        {
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
/// Beat builder. `seq` numbers item ids within one turn's namespace
/// (`t{turn}-{seq}`); continuation ARMS resume numbering from hand-picked
/// bases (90/95/80/85/89/70 at their park sites) chosen to stay clear of
/// the parent script's low sequence range — a new arm must pick a base
/// that cannot collide with ids the same turn already emitted.
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
        self.stream_at(text, STREAM_MS);
    }

    /// stream at an explicit pace (auto-resume streams at 20 ms).
    fn stream_at(&mut self, text: &str, pace_ms: u64) {
        let item_id = self.id("msg");
        self.emit(EventPayload::Item(ItemEvent::Started {
            item_id: item_id.clone(),
            item: TurnItem::AgentMessage {
                text: String::new().into(),
            },
        }));
        for token in split_word_tokens(text) {
            self.emit(EventPayload::Item(ItemEvent::Delta {
                item_id: item_id.clone(),
                delta: ItemDelta::Text {
                    text: token.clone().into(),
                },
            }));
            self.beats.push(Beat::Tokens {
                n: js_tokens(&token, 9),
                output: true,
            });
            self.sleep(pace_ms);
        }
        self.emit(EventPayload::Item(ItemEvent::Completed {
            item_id,
            item: TurnItem::AgentMessage {
                text: text.to_owned().into(),
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

/// The turn PREAMBLE (sim tui.js:1254-1260): the user row, its tokens,
/// THINKING for 750 ms, STREAMING — then `Dispatch`, which hands the branch
/// choice back to the driver. Splitting here is what makes the sim's
/// ordering exact: `low` is only examined after the think window, so an
/// interrupt inside it burns neither a GENERIC intro nor a roster callsign
/// (review P2-11).
#[must_use]
pub fn respond_preamble(
    user_text: &str,
    voice: bool,
    mode: haider_protocol::DeliveryMode,
    turn: u64,
) -> Vec<Beat> {
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
    b.beats.push(Beat::Tokens {
        n: js_tokens(user_text, 9),
        output: false,
    });
    b.state(RunState::Thinking);
    b.sleep(THINK_MS);
    b.streaming();
    b.beats.push(Beat::Dispatch {
        text: user_text.to_owned(),
        voice,
        turn,
    });
    b.beats
}

/// Build the branch body for user text (sim tui.js:1262-1546), chosen at
/// DISPATCH time. `generic` is the sim's `genRef` (the generic branch
/// post-increments; the test branch reads without incrementing) and
/// `roster` is `rosterRef` (claims post-increment) — both shared counters
/// so they advance exactly when the sim's do.
#[must_use]
pub fn respond_branch(
    user_text: &str,
    voice: bool,
    turn: u64,
    generic: &std::sync::atomic::AtomicU64,
    roster: &std::sync::atomic::AtomicU64,
) -> Vec<Beat> {
    use std::sync::atomic::Ordering::SeqCst;
    let low = user_text.to_lowercase();
    let mut b = B::new(turn);
    // The branch body re-enters STREAMING ids after the preamble's, so
    // start the sequence past them (the preamble emitted none).
    // Branch 1's own gate is a BARE /subagent/ (tui.js:1262) — only the
    // PLURAL detection carries `\b` (see `branch_subagent`).
    if low.contains("subagent") {
        branch_subagent(&mut b, &low, roster);
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
        branch_auth(&mut b, roster);
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
    } else if ["test", "flake", "fail"].iter().any(|k| low.contains(k)) || suffix_match(&low, "ci")
    {
        branch_test(&mut b, generic.load(SeqCst));
    } else if rate_limit_match(&low) || word_match(&low, "429") || low.contains("quota") {
        branch_rate_limit(&mut b);
    } else if low.contains("plan todo") {
        branch_plan_todo(&mut b);
    } else {
        branch_generic(&mut b, user_text, &low, generic.fetch_add(1, SeqCst));
    }
    let _ = voice;
    // NB: no trailing `Voice(false)` beat — a branch that parks on
    // `AwaitMenu` never reaches it, which left later ordinary rows tagged
    // `♪ speaking` (review P2-10). The voice tag now clears where it truly
    // ends: on the turn's TERMINAL run state, in the reducer.
    b.beats.push(Beat::TurnEnd);
    b.beats
}

/// The full turn (preamble + branch) — kept for the beat-level tests and
/// any caller that does not need the dispatch split.
#[must_use]
pub fn respond_beats(
    user_text: &str,
    voice: bool,
    mode: haider_protocol::DeliveryMode,
    turn: u64,
    generic: &std::sync::atomic::AtomicU64,
    roster: &std::sync::atomic::AtomicU64,
) -> Vec<Beat> {
    let mut beats = respond_preamble(user_text, voice, mode, turn);
    beats.pop(); // the Dispatch marker
    beats.extend(respond_branch(user_text, voice, turn, generic, roster));
    beats
}

/// §1.1 `/subagent/` (tui.js:1262-1290): the parent turn spawns LIVE
/// chips; `childRunTests`/`childRunDocs` play CONCURRENTLY while the
/// parent keeps going — the turn ends with children still running, so the
/// derived `◔ WAITING · N subagent(s)` badge takes over (§2.6).
fn branch_subagent(b: &mut B, low: &str, roster_counter: &std::sync::atomic::AtomicU64) {
    let plural = suffix_match(low, "subagents");
    let tests_name = claim_name(roster_counter);
    let tests_agent = format!("t{}-tests", b.turn);
    let docs_name = if plural {
        Some(claim_name(roster_counter))
    } else {
        None
    };
    if let Some(docs_name) = &docs_name {
        b.stream(&format!(
            "Spinning up two subagents — {} on the tests, {} on the docs. They run concurrently; click a chip under the input to watch one live.",
            tests_name.cs(),
            docs_name.cs(),
        ));
    } else {
        b.stream(&format!(
            "Spinning up a subagent — {} takes the test suite. It runs concurrently; click its chip under the input to watch it live.",
            tests_name.cs(),
        ));
    }
    b.tool(
        "agent_spawn",
        &format!("{} · tests → local · gpt-5.6", tests_name.callsign),
        800,
        "spawned · lease ok",
        true,
    );
    b.beats.push(Beat::ChipAdd(Box::new(ChipSeed {
        agent: tests_agent.clone(),
        parent: None,
        ros: Some(tests_name.ros),
        callsign: tests_name.callsign.clone(),
        hon: tests_name.hon,
        full: tests_name.full.clone(),
        name: "tests".to_owned(),
        model: "gpt-5.6".to_owned(),
        device: "local".to_owned(),
        state: ChipDisplayState::Idle,
        tokens: 1200,
        prefill: vec![],
    })));
    b.beats.push(Beat::ChipScript {
        agent: tests_agent.clone(),
        beats: child_run_tests(&tests_agent, b.turn),
    });
    if let Some(docs_name) = &docs_name {
        let docs_agent = format!("t{}-docs", b.turn);
        b.tool(
            "agent_spawn",
            &format!("{} · docs → local · gemini-3", docs_name.callsign),
            700,
            "spawned · lease ok",
            true,
        );
        b.beats.push(Beat::ChipAdd(Box::new(ChipSeed {
            agent: docs_agent.clone(),
            parent: None,
            ros: Some(docs_name.ros),
            callsign: docs_name.callsign.clone(),
            hon: docs_name.hon,
            full: docs_name.full.clone(),
            name: "docs".to_owned(),
            model: "gemini-3".to_owned(),
            device: "local".to_owned(),
            state: ChipDisplayState::Idle,
            tokens: 900,
            prefill: vec![],
        })));
        b.beats.push(Beat::ChipScript {
            agent: docs_agent.clone(),
            beats: child_run_docs(&docs_agent, b.turn),
        });
    }
    b.streaming();
    b.stream(
        "While they work I'm wiring the harness entrypoints here. The tests subagent will pause on a question — its chip flips to an amber ? when it does.",
    );
    b.tool("fs_edit", "src/core/harness.rs", 900, "+24 −6", true);
    b.streaming();
    b.stream(
        "My side is done. Chip glyphs: ○ idle · ◐ running · ⚒ tool · ? input required · ✓ done.",
    );
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
            presentation: None,
            option_actions: Vec::new(),
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

/// §1.4 `/auth|deleg|split|machin|device/` (tui.js:1345-1378): the
/// local chip is PRE-SEEDED (added before the spawn tool, transcript
/// pre-filled) and stepped tool → done between the parent's beats.
fn branch_auth(b: &mut B, roster_counter: &std::sync::atomic::AtomicU64) {
    let auth_name = claim_name(roster_counter);
    let auth_agent = format!("t{}-auth", b.turn);
    b.stream(&format!(
        "Splitting this: {} {} takes the service core locally while I wire the main side here.",
        auth_name.callsign, auth_name.hon,
    ));
    b.beats.push(Beat::ChipAdd(Box::new(ChipSeed {
        agent: auth_agent.clone(),
        parent: None,
        ros: Some(auth_name.ros),
        callsign: auth_name.callsign.clone(),
        hon: auth_name.hon,
        full: auth_name.full.clone(),
        name: "auth-svc".to_owned(),
        model: "gpt-5.6".to_owned(),
        device: "local".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 2400,
        prefill: vec![
            ChipPrefill::Note("· delegated locally — lease accepted, epoch 3".to_owned()),
            ChipPrefill::Agent(
                "Patching the service core locally; result returns as a fenced patch.".to_owned(),
            ),
            ChipPrefill::ToolOk {
                name: "fs_edit".to_owned(),
                desc: "svc/src/auth/core.rs".to_owned(),
                meta: "+88 −17".to_owned(),
            },
        ],
    })));
    b.tool(
        "agent_spawn",
        &format!("{} · auth-svc → local · gpt-5.6", auth_name.callsign),
        900,
        "lease accepted · epoch 3",
        true,
    );
    b.beats.push(Beat::ChipState {
        agent: auth_agent.clone(),
        state: ChipDisplayState::Tool,
    });
    b.tool("fs_edit", "web/src/lib/session.ts", 800, "+41 −7", true);
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
    b.beats.push(Beat::ChipState {
        agent: auth_agent,
        state: ChipDisplayState::Done,
    });
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
    b.tool("fs_edit", "tests/fixtures/clock.rs", 700, "+9 −3", true);
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
        ("fs_edit", "src/core/run_loop.rs", 900, "+42 −9"),
        ("fs_edit", "src/core/waiting.rs", 800, "+18 −3"),
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
    b.tool("fs_edit", "src/core/harness.rs", 850, "+37 −11", true);
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
/// `seq` makes the compaction item id UNIQUE per run: `/compact` twice
/// without token growth used to reuse `compact-{before}`, and the
/// projection — which permanently rejects closed item ids — dropped the
/// second row (review P2-13).
#[must_use]
pub fn compaction_beats(before: u64, after: u64, manual: bool, seq: u64) -> Vec<Beat> {
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
        item_id: ItemId::new(format!("compact-{seq}")),
        item: TurnItem::ContextCompaction {
            summary_artifact: haider_protocol::ids::ArtifactRef::new("blake3:demo-compact"),
            tokens_before: Some(before),
            tokens_after: Some(after),
            tokens_estimated: false,
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

// ---- §2 chip scripts (chipOps, tui.js:898-1074) ----

/// cStream: 18 ms/word, chip tokens `round(len*8)` ONCE at stream end.
///
/// `ns` is the chip's PER-TURN id namespace (`{agent}-t{turn}`). The sim
/// mints a fresh `nid()` per row (tui.js:900); the Rust port derived ids
/// from the agent plus a fixed suffix, so a SECOND message to the same chip
/// reused `…-g1`/`…-n2` — and the projection, which permanently rejects
/// closed item ids, silently dropped every assistant and tool row of that
/// turn (review P1-4). The namespace mirrors the main engine's `t{turn}-`.
fn chip_stream(beats: &mut Vec<Beat>, agent: &str, ns: &str, id: &str, text: &str) {
    let item_id = ItemId::new(format!("{ns}-{id}"));
    beats.push(Beat::ChipEmit {
        agent: agent.to_owned(),
        payload: EventPayload::Item(ItemEvent::Started {
            item_id: item_id.clone(),
            item: TurnItem::AgentMessage {
                text: String::new().into(),
            },
        }),
    });
    for token in split_word_tokens(text) {
        beats.push(Beat::ChipEmit {
            agent: agent.to_owned(),
            payload: EventPayload::Item(ItemEvent::Delta {
                item_id: item_id.clone(),
                delta: ItemDelta::Text { text: token.into() },
            }),
        });
        beats.push(Beat::Sleep(CHIP_STREAM_MS));
    }
    beats.push(Beat::ChipEmit {
        agent: agent.to_owned(),
        payload: EventPayload::Item(ItemEvent::Completed {
            item_id,
            item: TurnItem::AgentMessage {
                text: text.to_owned().into(),
            },
        }),
    });
    beats.push(Beat::ChipTokens {
        agent: agent.to_owned(),
        n: js_tokens(text, 8),
    });
}

/// cTool: running row → sleep dur → patched row, chip tokens +1800.
#[allow(clippy::too_many_arguments)]
fn chip_tool(
    beats: &mut Vec<Beat>,
    agent: &str,
    ns: &str,
    id: &str,
    name: &str,
    desc: &str,
    dur: u64,
    meta: &str,
    ok: bool,
) {
    let item_id = ItemId::new(format!("{ns}-{id}"));
    beats.push(Beat::ChipEmit {
        agent: agent.to_owned(),
        payload: EventPayload::Item(ItemEvent::Started {
            item_id: item_id.clone(),
            item: TurnItem::ToolCall {
                call_id: item_id.as_str().to_owned(),
                name: name.to_owned(),
                args: serde_json::json!({ "desc": desc }),
                status: ToolStatus::InProgress,
            },
        }),
    });
    beats.push(Beat::Sleep(dur));
    beats.push(Beat::ChipEmit {
        agent: agent.to_owned(),
        payload: EventPayload::Item(ItemEvent::Completed {
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
        }),
    });
    beats.push(Beat::ChipTokens {
        agent: agent.to_owned(),
        n: 1800,
    });
}

fn chip_state(beats: &mut Vec<Beat>, agent: &str, state: ChipDisplayState) {
    beats.push(Beat::ChipState {
        agent: agent.to_owned(),
        state,
    });
}

/// A parent-transcript tool row pushed already-complete (the collect rows).
fn parent_tool_done(beats: &mut Vec<Beat>, id: &str, name: &str, desc: &str, meta: &str) {
    let item_id = ItemId::new(id.to_owned());
    beats.push(Beat::Emit(EventPayload::Item(ItemEvent::Completed {
        item_id: item_id.clone(),
        item: TurnItem::ToolCall {
            call_id: item_id.as_str().to_owned(),
            name: name.to_owned(),
            args: serde_json::json!({ "desc": desc, "meta": meta }),
            status: ToolStatus::Completed,
        },
    })));
}

/// The chip question card routed into the CHIP's transcript — protocol
/// Menu with `MenuScope::Subagent` (the scope exists precisely for this).
/// The `ChipQuestion` beat carries the state too (see its doc comment):
/// callers must NOT emit a separate `ChipState` before it.
fn chip_question_menu(
    beats: &mut Vec<Beat>,
    agent: &str,
    recovery: bool,
    text: &str,
    options: &[&str],
) -> MenuId {
    // ORDER MATTERS: the protocol card opens FIRST, then the atomic
    // state+question patch. `ChipModel::question_menu` gates on all three,
    // so the card becomes visible exactly once — an observer can never
    // catch `input_required` with its question but without its menu.
    let menu_id = MenuId::new(format!("{agent}-q"));
    let kind = if recovery {
        MenuKind::Recovery {
            effect: EffectId::new(format!("e-{agent}")),
            presentation: None,
            option_actions: Vec::new(),
        }
    } else {
        MenuKind::Question
    };
    beats.push(Beat::ChipEmit {
        agent: agent.to_owned(),
        payload: EventPayload::MenuOpened(Menu {
            id: menu_id.clone(),
            kind,
            title: text.to_owned(),
            body: vec![],
            options: options
                .iter()
                .enumerate()
                .map(|(index, label)| option(&format!("o{index}"), label))
                .collect(),
            blocking: false,
            scope: MenuScope::Subagent {
                agent: haider_protocol::ids::AgentId::new(agent),
            },
            origin: "subagent".to_owned(),
            ttl_ms: None,
            timeout_option: None,
        }),
    });
    beats.push(Beat::ChipQuestion {
        agent: agent.to_owned(),
        recovery,
        text: text.to_owned(),
        options: options.iter().map(|o| (*o).to_owned()).collect(),
    });
    menu_id
}

/// childRunTests (tui.js:921-937): scoping → patch → the amber `?`
/// question; the answer arm finishes the suite and resumes the parent.
#[must_use]
pub fn child_run_tests(agent: &str, turn: u64) -> Vec<Beat> {
    let ns = format!("{agent}-t{turn}");
    let mut beats = Vec::new();
    chip_state(&mut beats, agent, ChipDisplayState::Running);
    chip_stream(
        &mut beats,
        agent,
        &ns,
        "s1",
        "Picking up the lease — scoping the billing test surface before writing anything.",
    );
    chip_tool(
        &mut beats,
        agent,
        &ns,
        "t1",
        "fs_read",
        "cloud/tests/billing/mod.rs",
        700,
        "212 lines",
        true,
    );
    chip_state(&mut beats, agent, ChipDisplayState::Tool);
    chip_tool(
        &mut beats,
        agent,
        &ns,
        "t2",
        "fs_edit",
        "cloud/tests/billing/webhooks.rs",
        900,
        "+96 −4",
        true,
    );
    let options = [
        "testcontainers — real db, slower",
        "mocks — fast, less coverage",
    ];
    let menu_id = chip_question_menu(
        &mut beats,
        agent,
        false,
        "Run the suite against testcontainers or mocks?",
        &options,
    );
    beats.push(Beat::Note(
        "· subagent tests needs input — its chip is holding an amber ? — click it to answer"
            .to_owned(),
    ));
    let arm = |choice: usize| -> Vec<Beat> {
        // Sim answerChip: state → running AND the question resolved in ONE
        // patch, then the chosen option + note rows (tui.js:1057-1063).
        let mut arm_beats = vec![Beat::ChipResolve {
            agent: agent.to_owned(),
            state: ChipDisplayState::Running,
        }];
        arm_beats.push(Beat::ChipEmit {
            agent: agent.to_owned(),
            payload: EventPayload::UserMessage {
                text: options[choice].to_owned(),
                attachments: vec![],
                mode: haider_protocol::DeliveryMode::Steer,
            },
        });
        arm_beats.push(Beat::ChipNote {
            agent: agent.to_owned(),
            text: "· input resolved — continuing".to_owned(),
        });
        arm_beats.push(Beat::Sleep(600));
        let cmd = if choice == 0 {
            "cargo test -p billing --tests -- --ignored"
        } else {
            "cargo test -p billing --tests"
        };
        chip_tool(
            &mut arm_beats,
            agent,
            &ns,
            "t3",
            "process_exec",
            cmd,
            1400,
            "41 passed",
            true,
        );
        chip_state(&mut arm_beats, agent, ChipDisplayState::Done);
        parent_tool_done(
            &mut arm_beats,
            &format!("t{turn}-collect-tests"),
            "agent_control",
            "collect tests → report accepted",
            "✓",
        );
        arm_beats.push(Beat::Note(
            "· subagent tests finished — report merged".to_owned(),
        ));
        arm_beats.push(Beat::AutoResume);
        arm_beats
    };
    beats.push(Beat::AwaitMenu {
        menu: menu_id,
        arms: vec![arm(0), arm(1)],
    });
    beats
}

/// childRunDocs (tui.js:940-958): a deliberate failure + `⌁` recovery.
#[must_use]
pub fn child_run_docs(agent: &str, turn: u64) -> Vec<Beat> {
    let ns = format!("{agent}-t{turn}");
    let mut beats = Vec::new();
    chip_state(&mut beats, agent, ChipDisplayState::Running);
    chip_stream(
        &mut beats,
        agent,
        &ns,
        "s1",
        "Drafting API docs for the new webhook endpoint from the patched source.",
    );
    chip_tool(
        &mut beats,
        agent,
        &ns,
        "t1",
        "fs_read",
        "cloud/src/billing/webhooks.rs",
        800,
        "381 lines",
        true,
    );
    chip_state(&mut beats, agent, ChipDisplayState::Tool);
    chip_tool(
        &mut beats,
        agent,
        &ns,
        "t2",
        "fs_edit",
        "docs/api/billing-webhooks.md",
        1100,
        "+140 −0",
        true,
    );
    chip_tool(
        &mut beats,
        agent,
        &ns,
        "t3",
        "process_exec",
        "cargo doc --no-deps",
        900,
        "exit 101 · docs feature flag missing",
        false,
    );
    let options = [
        "retry with --features docs",
        "close this subagent — keep the patch",
    ];
    let menu_id = chip_question_menu(
        &mut beats,
        agent,
        true,
        "cargo doc failed (exit 101 — the docs feature flag is missing). How should I recover?",
        &options,
    );
    beats.push(Beat::Note(
        "· subagent docs failed (✗) — open its row to pick a recovery".to_owned(),
    ));
    let mut retry = vec![Beat::ChipResolve {
        agent: agent.to_owned(),
        state: ChipDisplayState::Running,
    }];
    retry.push(Beat::ChipEmit {
        agent: agent.to_owned(),
        payload: EventPayload::UserMessage {
            text: options[0].to_owned(),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        },
    });
    retry.push(Beat::ChipNote {
        agent: agent.to_owned(),
        text: "· retrying with the fix".to_owned(),
    });
    retry.push(Beat::Sleep(700));
    chip_tool(
        &mut retry,
        agent,
        &ns,
        "t4",
        "process_exec",
        "cargo doc --no-deps --features docs",
        1300,
        "docs built ✓",
        true,
    );
    chip_state(&mut retry, agent, ChipDisplayState::Done);
    parent_tool_done(
        &mut retry,
        &format!("t{turn}-collect-docs"),
        "agent_control",
        "collect docs → report accepted",
        "✓",
    );
    retry.push(Beat::AutoResume);
    // idx 1: close the chip — the close lifecycle owns the note, the 5 s
    // removal and the resume check.
    let close = vec![Beat::ChipClose {
        agent: agent.to_owned(),
    }];
    beats.push(Beat::AwaitMenu {
        menu: menu_id,
        arms: vec![retry, close],
    });
    beats
}

/// respondChip (tui.js:1097-1161): a full turn on the CHIP's state
/// machine. The nested-delegation path ports the sim AS SHIPPED (review
/// r2 P2-14 adjudication: tui.js wins over the spec): the sim
/// early-returns at tui.js:1137 (`if (!(await ops.cTool(...)))` where
/// cTool returns undefined), leaving parent `streaming` and child
/// `running` forever. We reproduce that dead-end exactly; the INTENDED
/// flow (spawn → collect → integrate) is described at the early-return
/// site below and must NOT be "fixed" here unless the sim changes first.
#[must_use]
pub fn respond_chip_beats(
    agent: &str,
    chip_callsign: &str,
    chip_model: &str,
    chip_device: &str,
    text: &str,
    turn: u64,
    roster_counter: &std::sync::atomic::AtomicU64,
) -> Vec<Beat> {
    let low = text.to_lowercase();
    // Per-TURN id namespace — see `chip_stream` (review P1-4).
    let ns = format!("{agent}-t{turn}");
    let mut beats = Vec::new();
    beats.push(Beat::ChipEmit {
        agent: agent.to_owned(),
        payload: EventPayload::UserMessage {
            text: text.to_owned(),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        },
    });
    // Sim: `{ state: "thinking", question: null }` — one patch, so the
    // pending card can never survive the steer that cleared it.
    beats.push(Beat::ChipQuestionClear {
        agent: agent.to_owned(),
        state: ChipDisplayState::Thinking,
    });
    beats.push(Beat::Sleep(CHIP_THINK_MS));
    let nested = [
        "subagent", "delegate", "split", "spawn", "fan out", "parallel",
    ]
    .iter()
    .any(|k| low.contains(k));
    if nested {
        chip_state(&mut beats, agent, ChipDisplayState::Streaming);
        chip_stream(
            &mut beats,
            agent,
            &ns,
            "n1",
            "Good call — I'll delegate part of this to a child agent and wait on its result.",
        );
        let child_name = claim_name(roster_counter);
        let child_agent = format!("{agent}-sub{turn}");
        beats.push(Beat::ChipAdd(Box::new(ChipSeed {
            agent: child_agent.clone(),
            parent: Some(agent.to_owned()),
            ros: Some(child_name.ros),
            callsign: child_name.callsign.clone(),
            hon: child_name.hon,
            full: child_name.full.clone(),
            name: "subtask".to_owned(),
            model: chip_model.to_owned(),
            device: chip_device.to_owned(),
            state: ChipDisplayState::Running,
            tokens: 400,
            prefill: vec![ChipPrefill::Note(format!(
                "· spawned by {chip_callsign} — nested delegation"
            ))],
        })));
        chip_tool(
            &mut beats,
            agent,
            &ns,
            "n2",
            "agent_spawn",
            &format!("{} → {chip_device} · nested", child_name.callsign),
            700,
            "lease ok",
            true,
        );
        // ⚠ SIM BUG, PORTED AS-IS (tui.js:1137). The shipped sim wraps the
        // spawn in `if (!(await ops.cTool(...))) return;` — but `cTool`
        // resolves to `undefined`, so the guard ALWAYS fires and the turn
        // dead-ends right here: the parent chip stays `streaming`, the
        // nested child stays `running`, and because a live descendant keeps
        // the tree live the session shows `◔ WAITING` until the chip is
        // closed. Everything below the return in the sim is dead code.
        //
        // For the record, the beats the sim WOULD have run had `cTool`
        // returned true are: parent → waiting; child thinking → 500 ms →
        // streaming → "On it — scoped the subtask, patching now." → tool →
        // fs_edit src/subtask.rs 1100 ms +40 −6 → child done; parent →
        // streaming → "{callsign} {hon} finished — folded its patch into my
        // work. Done." → parent done → autoResumeParent. That flow is NOT
        // implemented here and must not be: tui.js is the authority.
    } else {
        chip_state(&mut beats, agent, ChipDisplayState::Streaming);
        chip_stream(
            &mut beats,
            agent,
            &ns,
            "g1",
            "Acknowledged — folding that into the current step.",
        );
        chip_state(&mut beats, agent, ChipDisplayState::Tool);
        chip_tool(
            &mut beats,
            agent,
            &ns,
            "g2",
            "fs_read",
            "src/target.rs",
            700,
            "read ok",
            true,
        );
        chip_state(&mut beats, agent, ChipDisplayState::Done);
        beats.push(Beat::AutoResume);
    }
    beats
}

/// autoResumeParent's script (§2.7, guards already checked): note →
/// THINKING 750 → STREAMING → 20 ms stream → the turn-end law.
#[must_use]
pub fn auto_resume_beats(reports: usize, turn: u64) -> Vec<Beat> {
    let mut b = B::new(turn);
    b.seq = 70;
    b.note("· all subagents reported — resuming the parked turn (waiting → thinking, never idle)");
    b.state(RunState::Thinking);
    b.sleep(THINK_MS);
    b.streaming();
    let what = if reports > 1 {
        format!("the {reports} subagent reports")
    } else {
        "the subagent report".to_owned()
    };
    b.stream_at(
        &format!(
            "Folding {what} into the main line — results merged, and the turn can now commit."
        ),
        RESUME_STREAM_MS,
    );
    b.beats.push(Beat::TurnEnd);
    b.beats
}

// ---- §3 aura scripts (tui.js:2058-2156) ----

/// The status-branch matcher:
/// `/status|what.*(doing|going|happen)|where|report|roster/`.
#[must_use]
pub fn aura_is_status(low: &str) -> bool {
    if ["status", "where", "report", "roster"]
        .iter()
        .any(|k| low.contains(k))
    {
        return true;
    }
    low.find("what").is_some_and(|at| {
        let rest = &low[at..];
        ["doing", "going", "happen"]
            .iter()
            .any(|k| rest.contains(k))
    })
}

/// The spawn-branch target: local placement plus the first matching service
/// stem. Device-like words in old/demo prompts never enable remote placement.
#[must_use]
pub fn aura_target(low: &str) -> (String, String) {
    let device = "local".to_owned();
    const STEMS: [&str; 9] = [
        "billing", "auth", "cellular", "payments", "web", "api", "docs", "search", "infra",
    ];
    let mut name = "service".to_owned();
    let mut best: Option<usize> = None;
    for stem in STEMS {
        let mut start = 0;
        while let Some(pos) = low[start..].find(stem) {
            let at = start + pos;
            let boundary = at == 0
                || !low[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            if boundary && best.is_none_or(|b| at < b) {
                // Extend through the word (`\w*`).
                let tail = low[at + stem.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>();
                name = format!("{stem}{tail}");
                best = Some(at);
            }
            start = at + stem.len();
        }
    }
    (name, device)
}

/// Aura stream: 16 ms/word into the aura transcript (no token meter).
fn aura_stream(beats: &mut Vec<Beat>, id: &str, text: &str) {
    let item_id = ItemId::new(format!("aura-{id}"));
    beats.push(Beat::AuraEmit(EventPayload::Item(ItemEvent::Started {
        item_id: item_id.clone(),
        item: TurnItem::AgentMessage {
            text: String::new().into(),
        },
    })));
    for token in split_word_tokens(text) {
        beats.push(Beat::AuraEmit(EventPayload::Item(ItemEvent::Delta {
            item_id: item_id.clone(),
            delta: ItemDelta::Text { text: token.into() },
        })));
        beats.push(Beat::Sleep(AURA_STREAM_MS));
    }
    beats.push(Beat::AuraEmit(EventPayload::Item(ItemEvent::Completed {
        item_id,
        item: TurnItem::AgentMessage {
            text: text.to_owned().into(),
        },
    })));
}

/// The status branch (tui.js:2078-2084). `summary` is built by the driver
/// from the live roster at submit time.
#[must_use]
pub fn aura_status_beats(spoken: bool, summary: &str, run: u64) -> Vec<Beat> {
    let mut beats = vec![
        Beat::AuraVoice(spoken),
        Beat::AuraState(AuraState::Thinking),
        Beat::Sleep(450),
        Beat::AuraState(AuraState::Speaking),
    ];
    aura_stream(
        &mut beats,
        &format!("r{run}-status"),
        &format!("Current roster: {summary}. Say the word to spin up more."),
    );
    beats.push(Beat::AuraState(AuraState::Idle));
    beats.push(Beat::AuraVoice(false));
    beats
}

/// The spawn branch (tui.js:2086-2124), verbatim strings and timings.
#[must_use]
pub fn aura_spawn_beats(spoken: bool, name: &str, _requested_device: &str, run: u64) -> Vec<Beat> {
    let device = "local";
    let mut beats = vec![
        Beat::AuraVoice(spoken),
        Beat::AuraState(AuraState::Thinking),
        Beat::Sleep(500),
    ];
    aura_stream(
        &mut beats,
        &format!("r{run}-plan"),
        &format!(
            "On it — I'll start a local {name} session, begin the work, and report back. I don't touch the code myself."
        ),
    );
    beats.push(Beat::AuraState(AuraState::Orchestrating));
    beats.push(Beat::AuraRosterPush {
        name: name.to_owned(),
        device: device.to_owned(),
    });
    beats.push(Beat::AuraLog(format!(
        "agent_spawn — {name} → {device} · lease ok"
    )));
    beats.push(Beat::Sleep(1100));
    beats.push(Beat::AuraRosterPatch {
        name: name.to_owned(),
        state: None,
        activity: "running the suite".to_owned(),
    });
    beats.push(Beat::AuraLog(format!("agent_control — {name}: run tests")));
    beats.push(Beat::Sleep(1300));
    beats.push(Beat::AuraRosterPatch {
        name: name.to_owned(),
        state: Some(ChipDisplayState::Done),
        activity: "tests green".to_owned(),
    });
    beats.push(Beat::AuraLog(format!("{name} on {device}: tests green ✓")));
    beats.push(Beat::AuraState(AuraState::Speaking));
    aura_stream(
        &mut beats,
        &format!("r{run}-done"),
        &format!(
            "Done — {name} is live on {device} and its tests are green. Open it, or spin up another?"
        ),
    );
    beats.push(Beat::AuraState(AuraState::Idle));
    beats.push(Beat::AuraVoice(false));
    beats
}
