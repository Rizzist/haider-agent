//! The app model + reducer: one owner of all TUI state, driven by a single
//! event stream (research rec 3/6). Rendering reads this model; nothing else
//! mutates it. The reducer is pure enough to test headlessly.

use crate::commands::{PALETTE_MAX_ROWS, PaletteItem, has_arg_slots, palette_items};
use crate::mock::seed_session_states;
use crate::projection::SessionProjection;
use crate::sanctum::SanctumTier;
use crate::script::{AuraState, ChipDisplayState, ChipPrefill, ChipSeed, TALK_PHRASE};
use crate::theme::ThemeKey;
use haider_protocol::ids::MenuId;
use haider_protocol::menu::{
    AnswerVia, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::state::{HarnessStatus, RunState};
use haider_protocol::{DeliveryMode, EventPayload};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;

/// Sim `autoBlurb` (tui.js:401-406): strip a leading slash-command token,
/// keep the first seven words, cap at 46 chars, capitalize the first letter.
#[must_use]
pub fn auto_blurb(text: &str) -> String {
    let body: String = if text.starts_with('/') {
        text.split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        text.to_owned()
    };
    let joined = body
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ");
    let truncated = if joined.chars().count() > 46 {
        let cut: String = joined.chars().take(46).collect();
        format!("{}…", cut.trim_end())
    } else {
        joined
    };
    let mut chars = truncated.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "New session".to_owned(),
    }
}

/// Sim session-name slug (tui.js:2014-2016): first 3 words, joined by `-`,
/// lowercased, `[a-z0-9-]` only, max 28 chars, fallback `session`.
#[must_use]
pub fn slug_name(text: &str) -> String {
    let joined = text
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    let slug: String = joined
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(28)
        .collect();
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    }
}

/// The shell builtins the demo VFS serves locally — instant, NO model turn
/// (sim `SHELL_CMDS`, tui.js:1993-2008).
pub const SHELL_CMDS: [&str; 6] = ["ls", "dir", "pwd", "cd", "mkdir", "touch"];

/// The demo VFS seed (sim tui.js:418-426).
#[must_use]
pub fn vfs_seed() -> BTreeMap<String, Vec<String>> {
    let entry = |dir: &str, names: &[&str]| {
        (
            dir.to_owned(),
            names.iter().map(|n| (*n).to_owned()).collect(),
        )
    };
    BTreeMap::from([
        entry(
            "~/dev",
            &[
                "diffforge/",
                "enterprise-suite/",
                "haider-code/",
                "notes.md",
            ],
        ),
        entry(
            "~/dev/diffforge",
            &["cloud/", "cellular/", "web/", "README.md"],
        ),
        entry(
            "~/dev/diffforge/cloud",
            &["src/", "tests/", "docs/", "Cargo.toml"],
        ),
        entry("~/dev/diffforge/cellular", &["src/", "pbx/", "Cargo.toml"]),
        entry("~/dev/diffforge/web", &["src/", "public/", "package.json"]),
        entry(
            "~/dev/enterprise-suite",
            &["services/", "web/", "infra/", "README.md"],
        ),
        entry("~/dev/haider-code", &["PROPOSAL.md", "research/"]),
    ])
}

/// Sim `resolvePath` (tui.js:444-462): `~` roots, `.` no-ops, `..` pops
/// with a one-segment floor; empty targets default to `~/dev`.
#[must_use]
pub fn resolve_path(arg: &str, cwd: &str) -> String {
    if arg.is_empty() {
        return "~/dev".to_owned();
    }
    if arg.starts_with('~') {
        let segments: Vec<&str> = arg.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return "~".to_owned();
        }
        return segments.join("/");
    }
    let mut base: Vec<String> = cwd
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    for segment in arg.split('/').filter(|s| !s.is_empty()) {
        match segment {
            "." => {}
            ".." => {
                if base.len() > 1 {
                    base.pop();
                }
            }
            other => base.push(other.to_owned()),
        }
    }
    base.join("/")
}

/// Unknown dirs list `src/ README.md` (sim tui.js:448).
fn default_listing() -> Vec<String> {
    vec!["src/".to_owned(), "README.md".to_owned()]
}

/// Sim `runShell` (tui.js:444-462) against the demo VFS. Returns the
/// output line and, for `cd`, the retargeted working dir.
#[must_use]
pub fn run_shell(
    line: &str,
    cwd: &str,
    vfs: &mut BTreeMap<String, Vec<String>>,
) -> (String, Option<String>) {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let arg = parts.next().unwrap_or("");
    match cmd.as_str() {
        "ls" | "dir" => {
            let entries = vfs.get(cwd).cloned().unwrap_or_else(default_listing);
            (entries.join("  "), None)
        }
        "pwd" => (cwd.to_owned(), None),
        "cd" => {
            let target = resolve_path(arg, cwd);
            (format!("→ {target}"), Some(target))
        }
        "mkdir" | "touch" => {
            if arg.is_empty() {
                return (format!("usage: {cmd} <name>"), None);
            }
            let entry = if cmd == "mkdir" {
                format!("{arg}/")
            } else {
                arg.to_owned()
            };
            let listing = vfs.entry(cwd.to_owned()).or_insert_with(default_listing);
            if listing.contains(&entry) {
                (format!("{entry} already exists"), None)
            } else {
                listing.push(entry.clone());
                (format!("created {entry}"), None)
            }
        }
        other => (format!("unknown: {other}"), None),
    }
}

/// Which screen is showing (sim: boot | main | session | sub | aura).
/// The subagent view's target chip lives in [`AppModel::view_path`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Boot,
    Launcher,
    Session,
    Subagent,
    Aura,
}

/// A chip's pending question (the amber `?` / recovery `⌁`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipQuestion {
    pub recovery: bool,
    pub text: String,
    pub options: Vec<String>,
    pub resolved: bool,
}

/// One subagent chip — the sim's recursive tree node (§2). Each chip owns
/// its own [`SessionProjection`]: "a child is the same object".
#[derive(Debug)]
pub struct ChipModel {
    pub agent: String,
    /// The roster index the callsign was claimed at (persistence guard 3:
    /// the reload's honour-roll restore reads every chip's `ros`).
    pub ros: Option<u64>,
    pub callsign: String,
    pub hon: &'static str,
    pub full: String,
    pub name: String,
    pub model: String,
    pub device: String,
    pub state: ChipDisplayState,
    pub tokens: u64,
    pub question: Option<ChipQuestion>,
    pub closed: bool,
    pub removing: bool,
    pub children: Vec<ChipModel>,
    pub transcript: SessionProjection,
}

impl ChipModel {
    #[must_use]
    pub fn from_seed(seed: ChipSeed) -> Self {
        let mut transcript = SessionProjection::new();
        for prefill in &seed.prefill {
            match prefill {
                ChipPrefill::Note(text) => transcript.push_note(text.clone()),
                ChipPrefill::Agent(text) => {
                    transcript.apply(&EventPayload::Item(
                        haider_protocol::item::ItemEvent::Completed {
                            item_id: haider_protocol::ids::ItemId::new(format!(
                                "{}-seed-a",
                                seed.agent
                            )),
                            item: haider_protocol::item::TurnItem::AgentMessage {
                                text: text.clone(),
                            },
                        },
                    ));
                }
                ChipPrefill::ToolOk { name, desc, meta } => {
                    transcript.apply(&EventPayload::Item(
                        haider_protocol::item::ItemEvent::Completed {
                            item_id: haider_protocol::ids::ItemId::new(format!(
                                "{}-seed-t",
                                seed.agent
                            )),
                            item: haider_protocol::item::TurnItem::ToolCall {
                                call_id: format!("{}-seed-t", seed.agent),
                                name: name.clone(),
                                args: serde_json::json!({ "desc": desc, "meta": meta }),
                                status: haider_protocol::item::ToolStatus::Completed,
                            },
                        },
                    ));
                }
            }
        }
        Self {
            agent: seed.agent,
            ros: seed.ros,
            callsign: seed.callsign,
            hon: seed.hon,
            full: seed.full,
            name: seed.name,
            model: seed.model,
            device: seed.device,
            state: seed.state,
            tokens: seed.tokens,
            question: None,
            closed: false,
            removing: false,
            children: Vec::new(),
            transcript,
        }
    }

    /// The chip's question card, per the sim's `chipMenu` gate
    /// (tui.js:2360-2364): open only while the chip is `input_required`/
    /// `error` AND holds an UNRESOLVED question. A closed chip has its
    /// question force-resolved, so its view shows the composer again even
    /// though the protocol Menu in its projection is still open.
    #[must_use]
    pub fn question_menu(&self) -> Option<&Menu> {
        if self.closed
            || !matches!(
                self.state,
                ChipDisplayState::InputRequired | ChipDisplayState::Error
            )
            || self.question.as_ref().is_none_or(|q| q.resolved)
        {
            return None;
        }
        self.transcript.open_menu()
    }

    /// `chipIsLive` (tui.js:286): not closed, state ∉ {done, error}.
    #[must_use]
    pub fn is_live(&self) -> bool {
        !self.closed && !matches!(self.state, ChipDisplayState::Done | ChipDisplayState::Error)
    }

    /// `chipDisplayState` (tui.js:2810-2811): a live chip that is NOT
    /// input_required with a live descendant displays `waiting`.
    #[must_use]
    pub fn display_state(&self) -> ChipDisplayState {
        if self.is_live()
            && self.state != ChipDisplayState::InputRequired
            && tree_live_count(&self.children) > 0
        {
            ChipDisplayState::Waiting
        } else {
            self.state
        }
    }

    /// `chipActivity` (tui.js:2825-2833), truncated at 52 chars + `…`.
    #[must_use]
    pub fn activity(&self) -> String {
        if self.closed {
            return "closing · leaves in 5s".to_owned();
        }
        if self.state == ChipDisplayState::InputRequired
            && let Some(question) = &self.question
            && !question.resolved
        {
            return truncate_activity(&question.text);
        }
        let live_children = tree_live_count(&self.children);
        if self.display_state() == ChipDisplayState::Waiting && live_children > 0 {
            let plural = if live_children > 1 {
                "children"
            } else {
                "child"
            };
            return format!("waiting on {live_children} {plural}");
        }
        if self.state == ChipDisplayState::Done {
            return "report ready".to_owned();
        }
        if self.state == ChipDisplayState::Thinking {
            return "thinking…".to_owned();
        }
        // Sim: NO entries → `starting…`; an entry with empty text → `…`
        // (tui.js:2377-2380).
        let last = self.transcript.entries().last().map(|entry| match entry {
            crate::projection::TranscriptEntry::Item(block) => match &block.item {
                haider_protocol::item::TurnItem::ToolCall { name, args, .. } => {
                    let desc = args
                        .get("desc")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    format!("{name} {desc}")
                }
                haider_protocol::item::TurnItem::AgentMessage { text } => text.clone(),
                _ => String::new(),
            },
            crate::projection::TranscriptEntry::User { text, .. } => text.clone(),
            crate::projection::TranscriptEntry::Note { text } => text.clone(),
            crate::projection::TranscriptEntry::Shell { cmd, .. } => format!("$ {cmd}"),
        });
        match last {
            Some(text) if text.is_empty() => "…".to_owned(),
            Some(text) => truncate_activity(&text),
            None => "starting…".to_owned(),
        }
    }
}

fn truncate_activity(text: &str) -> String {
    if text.chars().count() > 52 {
        format!("{}…", text.chars().take(52).collect::<String>())
    } else {
        text.to_owned()
    }
}

/// `treeLiveCount` (tui.js:286-329): live chips, recursively.
#[must_use]
/// The chips-level half of the close lifecycle (§2.5): flags + the parent
/// transcript note — shared by the attached surface (`close_chip_state`)
/// and background routing, so both speak one law. Returns `was_live`, or
/// `None` when the chip is unknown or already closed.
pub fn close_chip_core(
    chips: &mut [ChipModel],
    projection: &mut SessionProjection,
    agent: &str,
) -> Option<bool> {
    let chip = find_chip_mut(chips, agent)?;
    if chip.closed {
        return None;
    }
    let was_live = chip.is_live();
    chip.closed = true;
    chip.removing = true;
    if let Some(question) = &mut chip.question {
        question.resolved = true;
    }
    projection.push_note(format!(
        "· subagent {} {} closed — leaving the tree in 5s",
        chip.callsign, chip.hon
    ));
    Some(was_live)
}

pub fn tree_live_count(chips: &[ChipModel]) -> usize {
    chips
        .iter()
        .map(|chip| usize::from(chip.is_live()) + tree_live_count(&chip.children))
        .sum()
}

/// Any non-closed chip whose DISPLAYED state pulses in the sim (running /
/// tool maroon · input-required amber, tui.js:4823-4834), recursively.
/// Waiting (◔), done and error are deliberately still.
fn chips_animated(chips: &[ChipModel]) -> bool {
    chips.iter().any(|chip| {
        (!chip.closed
            && matches!(
                chip.display_state(),
                ChipDisplayState::Running
                    | ChipDisplayState::Tool
                    | ChipDisplayState::InputRequired
            ))
            || chips_animated(&chip.children)
    })
}

/// A tool row still in flight (sim ToolRow `.glyph` while
/// `$status === "running"`, tui.js:4524-4530). Scanned from the tail —
/// a live tool is always recent.
fn streaming_tool_live(entries: &[crate::projection::TranscriptEntry]) -> bool {
    use haider_protocol::item::{ToolStatus, TurnItem};
    entries.iter().rev().any(|entry| {
        matches!(
            entry,
            crate::projection::TranscriptEntry::Item(block)
                if matches!(
                    &block.item,
                    TurnItem::ToolCall { status: ToolStatus::InProgress | ToolStatus::Pending, .. }
                )
        )
    })
}

/// Find a chip anywhere in the tree.
#[must_use]
pub fn find_chip<'t>(chips: &'t [ChipModel], agent: &str) -> Option<&'t ChipModel> {
    for chip in chips {
        if chip.agent == agent {
            return Some(chip);
        }
        if let Some(found) = find_chip(&chip.children, agent) {
            return Some(found);
        }
    }
    None
}

pub fn find_chip_mut<'t>(chips: &'t mut [ChipModel], agent: &str) -> Option<&'t mut ChipModel> {
    for chip in chips {
        if chip.agent == agent {
            return Some(chip);
        }
        if let Some(found) = find_chip_mut(&mut chip.children, agent) {
            return Some(found);
        }
    }
    None
}

/// The root→chip path (breadcrumb + view addressing).
#[must_use]
pub fn path_to_chip(chips: &[ChipModel], agent: &str) -> Option<Vec<String>> {
    for chip in chips {
        if chip.agent == agent {
            return Some(vec![chip.agent.clone()]);
        }
        if let Some(mut path) = path_to_chip(&chip.children, agent) {
            path.insert(0, chip.agent.clone());
            return Some(path);
        }
    }
    None
}

/// Remove a chip (and its subtree) wherever it sits.
pub fn remove_chip(chips: &mut Vec<ChipModel>, agent: &str) -> bool {
    if let Some(index) = chips.iter().position(|chip| chip.agent == agent) {
        chips.remove(index);
        return true;
    }
    chips
        .iter_mut()
        .any(|chip| remove_chip(&mut chip.children, agent))
}

/// Depth-first flatten with depth (the SubTree rows).
#[must_use]
pub fn flatten_chips(chips: &[ChipModel]) -> Vec<(usize, &ChipModel)> {
    let mut rows = Vec::new();
    fn walk<'t>(chips: &'t [ChipModel], depth: usize, rows: &mut Vec<(usize, &'t ChipModel)>) {
        for chip in chips {
            rows.push((depth, chip));
            walk(&chip.children, depth + 1, rows);
        }
    }
    walk(chips, 0, &mut rows);
    rows
}

/// One controlled-session row on the aura stage.
#[derive(Debug, Clone)]
pub struct AuraAgentRow {
    pub name: String,
    pub device: String,
    pub state: ChipDisplayState,
    pub activity: String,
}

/// The aura orchestrator surface (§3 — demo-local; sim seedVoiceSession,
/// tui.js:121-138). Exiting the screen does NOT reset this state.
#[derive(Debug)]
pub struct AuraModel {
    /// true = gpt-realtime (native duplex); false = composed STT·LLM·TTS.
    pub realtime: bool,
    pub muted: bool,
    pub state: AuraState,
    pub roster: Vec<AuraAgentRow>,
    pub log: Vec<String>,
    pub transcript: SessionProjection,
    /// Per-run counter for unique stream item ids.
    pub runs: u64,
}

impl AuraModel {
    #[must_use]
    pub fn seed() -> Self {
        let mut transcript = SessionProjection::new();
        transcript.set_voice_live(true);
        transcript.apply(&EventPayload::Item(
            haider_protocol::item::ItemEvent::Completed {
                item_id: haider_protocol::ids::ItemId::new("aura-seed"),
                item: haider_protocol::item::TurnItem::AgentMessage {
                    text: "Aura online. I orchestrate sessions across your devices — I don't write code myself. Say or type what to spin up.".to_owned(),
                },
            },
        ));
        transcript.set_voice_live(false);
        Self {
            realtime: true,
            muted: false,
            state: AuraState::Idle,
            roster: vec![AuraAgentRow {
                name: "billing-service".to_owned(),
                device: "workstation".to_owned(),
                state: ChipDisplayState::Done,
                activity: "webhook tests green".to_owned(),
            }],
            log: vec![
                "spawned billing-service on workstation".to_owned(),
                "ran cargo test -p billing — 216 passed".to_owned(),
            ],
            transcript,
            runs: 0,
        }
    }

    /// `VOICE_ENGINES[engine].label` (tui.js:121-138).
    #[must_use]
    pub const fn engine_label(&self) -> &'static str {
        if self.realtime {
            "gpt-realtime-2"
        } else {
            "whisper → gpt-5.6 → openai"
        }
    }

    /// `VOICE_ENGINES[engine].kind`.
    #[must_use]
    pub const fn engine_kind(&self) -> &'static str {
        if self.realtime {
            "native duplex"
        } else {
            "STT·LLM·TTS"
        }
    }
}

impl Default for AuraModel {
    fn default() -> Self {
        Self::seed()
    }
}

/// Side effects the reducer requests from the runtime (the reducer itself
/// never performs IO).
#[derive(Debug, Clone, PartialEq)]
pub enum AppRequest {
    /// Run a respond() turn for user text. `voice` turns skip the script's
    /// UserMessage (the reducer already pushed the ◉ row); `title` asks the
    /// driver to schedule the 1.5 s auto-title micro-call, which names the
    /// session INSIDE its callback (sim tui.js:1219-1227, review P2-12).
    SubmitText {
        text: String,
        voice: bool,
        title: bool,
    },
    /// Cancel EVERY session's and every chip's arms and clear all demo
    /// token meters — a GLOBAL reset, not a polite stop (renamed from
    /// `StopScripts`, review TUI4.1 D3-4: the old name undersold the
    /// blast radius). Pushed only by [`AppModel::fresh_session`] — the
    /// `/reset` teardown and the scratch surface's fresh start. Aura
    /// deliberately survives (sim tui.js:1950-1955); `/reset` resets it
    /// separately via [`Self::ResetAura`].
    ResetAllSessions,
    /// Esc mid-turn: stop the playing script; the reducer already settled
    /// the projection into idle(i) (sim interrupt, tui.js:1551-1567).
    Interrupt,
    /// Manual `/compact` (sim tui.js:1791-1806).
    Compact,
    /// A drag selection finished (owner item 9): the RUNTIME extracts the
    /// selected text from its last-drawn frame and copies it (pbcopy, then
    /// OSC 52 — see [`crate::clipboard`]). A request because the reducer
    /// never sees the rendered buffer; headless tests assert the request
    /// itself.
    CopySelection,
    /// TUI5 items 4+5: copy MODEL-known text (the composer selection on
    /// ⌃C or drag-release). Unlike [`Self::CopySelection`] the reducer
    /// already holds the exact text, so it travels in the request; the
    /// runtime runs the same pbcopy + OSC 52 + honest-flash path.
    CopyText(String),
    /// The ◉ talk hold started — fire the canned phrase after 1300 ms.
    Talk,
    /// Steer/message a subagent (respondChip, §2.4) — a full turn on the
    /// CHIP's state machine.
    ChipSubmit { agent: String, text: String },
    /// Close a chip (✕ / the docs-recovery close arm): lifecycle flags are
    /// the reducer's; the driver owns the 5 s removal + resume timers.
    ChipClose { agent: String },
    /// Run an aura orchestrate turn (§3.4).
    AuraSubmit { text: String, voice: bool },
    /// The aura hold-to-talk: 1100 ms listening, then the canned phrase.
    AuraTalk,
    /// `/reset` reseeded the aura — bump its script guard.
    ResetAura,
    /// `/reset` also purges the demo state file (sim tui.js:1918:
    /// `localStorage.removeItem("haider-tui-v1")`). Runtime-owned like
    /// `CopySelection`: only the interactive loop knows the store path, so
    /// it intercepts this; the driver treats it as a no-op.
    PurgeDemoStore,
    /// Quit the app.
    Quit,
}

/// Per-session voice pipeline (sim `DEFAULT_VOICE`, tui.js:110 — voice
/// ships ON with Whisper STT → OpenAI TTS, non-duplex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceState {
    pub enabled: bool,
    pub stt: String,
    pub tts: String,
    pub duplex: bool,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            enabled: true,
            stt: "whisper-large-v3".to_owned(),
            tts: "openai-tts".to_owned(),
            duplex: false,
        }
    }
}

impl VoiceState {
    /// The status-bar segment (sim tui.js:2846-2850): duplex shows the
    /// engine name; else `{stt-first-word}→{tts-first-word}`.
    #[must_use]
    pub fn bar_label(&self) -> String {
        if self.duplex {
            return "gpt-realtime".to_owned();
        }
        let first = |s: &str| s.split('-').next().unwrap_or("").to_owned();
        format!("{}→{}", first(&self.stt), first(&self.tts))
    }
}

/// A clickable region's action (hit-testing: render reports regions, the
/// runtime maps clicks back through [`AppModel::handle_hit`]).
///
/// Hits carry VALUES, not row indices (review r2 P2-2): a click resolved
/// through the previous frame's map must activate exactly what was on
/// screen — or be dropped — never a different row the model drifted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// The launcher row's session NAME at render time (review P2-9: an
    /// ordinal resolved against current state could attach a different
    /// session than the one clicked).
    AttachSample(String),
    /// Aura / Accounts / Peers launcher rows, by identity not ordinal.
    ExtraRow(LauncherRow),
    /// The palette row's actual content at render time.
    PaletteRow(PaletteItem),
    /// A menu option, bound to the menu it was rendered for.
    MenuOption {
        menu: MenuId,
        index: usize,
    },
    BackChip,
    TalkChip,
    HelpHint,
    /// A SubTree row — opens the chip's own view.
    ChipRow(String),
    /// The SubTree header (collapse toggle).
    SubTreeToggle,
    /// The pinned-todos header (collapse toggle, owner item 7).
    TodosToggle,
    /// One pinned-todo row. Carries the todo's id so a stale rect can only
    /// ever light the row it was measured on. Clicking a row does nothing —
    /// the sim's rows are not buttons; the hit exists so the row can take
    /// hover chrome like every other list row.
    TodoRow(u32),
    /// `⌂ {session} — back to the main transcript` (subagent screen).
    SessionHome,
    /// The chip view's `✕ close`.
    ChipCloseBtn(String),
    /// A breadcrumb hop in the chip view (session root = empty path).
    ChipCrumb(Vec<String>),
    /// Aura stage chrome.
    AuraEngine,
    AuraMute,
    AuraExit,
    AuraTalkBtn,
    /// The sticky origin line — carries the scroll-back that puts the
    /// producing prompt's first row at the viewport top (sim jumpToSticky:
    /// stay AT the prompt, tui.js:2637-2645).
    StickyJump(u16),
    /// One composer text row (TUI5 item 5). Value-carrying like every
    /// hit: `start` is the ABSOLUTE byte offset (in the composer text) of
    /// the row's visible slice at render time, `content` the slice
    /// itself — a click maps its column through `content`'s graphemes, so
    /// a stale frame can only ever place the caret where that frame's
    /// cells actually were (or be dropped by the press guard).
    ComposerText {
        start: usize,
        content: String,
    },
}

/// One answer on its way to the client, tagged with the session identity that
/// RENDERED the card (review r2 P1-1). Answers ride the never-cancelled
/// control tag so delivery is guaranteed, but CONSUMPTION checks the
/// origin: an answer to a card the user has since replaced must never
/// reconfigure the session that took its place. The sim gets this for free
/// — its `askMenu` promise closes over the originating session/branch ids
/// and its menu ids are per-open `nid()`s (tui.js:849-878).
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundAnswer {
    pub origin: u64,
    pub answer: MenuAnswer,
}

impl std::ops::Deref for OutboundAnswer {
    type Target = MenuAnswer;
    fn deref(&self) -> &MenuAnswer {
        &self.answer
    }
}

/// The launcher's non-session rows (value-carrying hit payload, P2-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherRow {
    Aura,
    Accounts,
    Peers,
}

/// A composer surface's identity (TUI5 item 9): the launcher, one session
/// (by id — the monotonic-identity law means a key can never be reworn),
/// or the aura. The SUBAGENT screen shares its session's key (the
/// amendment's key list is exactly launcher | session id | aura), and the
/// scratch surface (screen=Session, no id) shares the launcher's —
/// documented: scratch is the launcher's envelope-driven lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DraftKey {
    Launcher,
    Session(u64),
    Aura,
}

/// Everything the reducer consumes.
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    /// Bracketed paste arrives atomically; newlines never submit (rec 14).
    Paste(String),
    /// Boxed: `EventPayload` is much larger than the other variants.
    Envelope(Box<EventPayload>),
    /// The demo script (or stream) ended.
    StreamEnded,
}

/// Identity shown in the status bar and launcher info line. Real values come
/// from config/accounts in later waves; the demo pins sim-parity defaults.
#[derive(Debug, Clone)]
pub struct IdentityLine {
    pub provider: String,
    pub model_short: String,
    pub account: String,
    pub device: String,
    pub context_window: u64,
}

impl Default for IdentityLine {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_owned(),
            model_short: "fable-5".to_owned(),
            account: "none · /login".to_owned(),
            device: "this-mac".to_owned(),
            context_window: 200_000,
        }
    }
}

/// The single mutable application state (research rec 3).
#[derive(Debug)]
pub struct AppModel {
    pub screen: Screen,
    pub theme: ThemeKey,
    pub sanctum_tier: SanctumTier,
    pub projection: SessionProjection,
    pub identity: IdentityLine,
    /// The ACTIVE surface's composer (TUI5): text + first-class cursor +
    /// selection + input ring. Nothing in it persists (item 8).
    pub composer: crate::composer::Composer,
    /// Parked composers for the surfaces NOT on screen (TUI5 item 9):
    /// every surface — launcher, each session, aura — keeps its own draft
    /// (text AND cursor/selection/ring travel together, Claude Code's
    /// per-conversation drafts). Navigation swaps through here; nothing
    /// in it persists (item 8's DTO assertion covers it).
    pub drafts: std::collections::HashMap<DraftKey, crate::composer::Composer>,
    /// Session blurb (sim auto-title micro-call) — announced by the 1.5 s
    /// `· session titled` note; the HEADER shows [`Self::session_name`].
    pub session_title: Option<String>,
    /// Session slug name (sim tui.js:2014-2016) — header + window title.
    pub session_name: Option<String>,
    /// Head callsign for the live demo session (sim: claimed from the
    /// roster at `newSession`, tui.js:1631 — TUI4c makes the claim real).
    pub session_head: (String, String),
    /// Mid-turn input held for turn end (sim queue mode, §4.4): the ⧗
    /// panel's rows; consumed by the driver's `finish_turn` with no idle.
    pub msg_queue: Vec<String>,
    /// `/queue turn` — mid-turn input queues instead of steering.
    pub queue_mode: bool,
    /// Per-session voice pipeline (sim DEFAULT_VOICE — ships ON).
    pub voice: VoiceState,
    /// The ◉ talk hold is live (`◉ listening…` chip + status segment).
    pub listening: bool,
    /// The launcher's working dir for shell builtins (sim `~/dev/enterprise-suite`).
    pub launcher_dir: String,
    /// The session's working dir — shown in the header; `cd` retargets it.
    pub session_dir: String,
    /// Per-open card counter: `/voice` and `/tools` mint a FRESH menu id
    /// each time, exactly as the sim's `nid()` does (review r2 P1-1 — fixed
    /// ids let a stale answer apply its consequences to a later card).
    pub card_seq: u64,
    /// The demo VFS the shell builtins run against (sim tui.js:418-426).
    pub vfs: BTreeMap<String, Vec<String>>,
    /// The launcher's `.shellout` block: last builtin (cmd, output).
    pub launcher_shellout: Option<(String, String)>,
    /// The session's subagent chip tree (§2 — demo-local).
    pub chips: Vec<ChipModel>,
    /// The chip path the subagent screen is viewing (breadcrumb).
    pub view_path: Vec<String>,
    /// The SubTree header collapse toggle (`▾`/`▸ subagents`).
    pub subtree_collapsed: bool,
    /// The pinned-todos header collapse toggle (sim tui.js:2863-2888 — the
    /// header is a button and the collapsed form summarises the current
    /// item; owner item 7 promotes it from the deferred ledger).
    pub todos_collapsed: bool,
    /// An auto-resume turn is in flight (§2.7 guard).
    pub auto_resuming: bool,
    /// The aura orchestrator surface (persists across screen exits).
    pub aura: AuraModel,
    /// EVERY session, fully materialized (sim `sessions`, tui.js:497) —
    /// seeds and user-created alike; newest user session first, then the
    /// seeds. The ATTACHED session's state is checked OUT of its slot into
    /// this model's live fields (see `crate::session`).
    pub sessions: Vec<crate::session::SessionState>,
    /// The checked-out session's id (sim `activeId`; `None` = launcher's
    /// no-session state, exactly the sim's `setActiveId(null)`).
    pub active_session: Option<u64>,
    /// The most recently detached session — the empty-⏎ re-attach target.
    pub last_detached: Option<u64>,
    /// Session id allocator (seeds take 1-3; sim uses `Date.now()`).
    /// MONOTONIC for the process lifetime — never reset, not even by
    /// `/reset` (review TUI4.1 P1-2): an id-keyed control callback must
    /// never find a replacement session wearing a dead session's id.
    pub next_session_id: u64,
    /// The roster claim counter (sim `rosterRef`, tui.js:681) — shared
    /// with the driver so heads and chips draw from ONE honour roll.
    pub roster: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Selected option index while a blocking menu replaces the composer.
    pub menu_selection: usize,
    /// Selected row in the slash palette (open while composer starts with /).
    /// Ranges over the FULL match list; the render window follows.
    pub palette_selection: usize,
    /// First visible palette row — the scroll window that keeps the
    /// selection visible (sim CmdMenu internal scroll, tui.js:2710-2718).
    pub palette_scroll: usize,
    /// Esc dismissed the palette without clearing the composer (sim
    /// `menuDismissed`); any composer edit re-opens it.
    pub palette_dismissed: bool,
    /// The /help overlay (esc closes).
    pub help_open: bool,
    /// One-line transient notice shown in the status bar until the next
    /// keystroke (honest stubs: "/tree lands with the daemon").
    pub flash: Option<String>,
    /// Answers the user produced; the runtime drains these to the client
    /// (side effects never happen inside the reducer).
    pub outbox: Vec<OutboundAnswer>,
    /// Reducer-requested side effects; the runtime drains these.
    pub requests: Vec<AppRequest>,
    /// True while a demo turn is playing (submits are ignored, honestly).
    pub turn_active: bool,
    /// Wheel scroll-back offset in the session transcript (0 = follow
    /// bottom; wheel up increases, wheel down decreases). A `Cell` because
    /// RENDER is the single scroll authority (review r3 P2-2). The wheel
    /// applies reconcile-then-apply (review r5 P2-2): fold to the
    /// ≤1-frame-stale [`Self::scroll_max`], then apply the notch clamped
    /// to it — bursts bank no debt; the frame's reconcile is the backstop.
    pub scroll_back: std::cell::Cell<u16>,
    /// Max scroll-back of the LAST rendered frame — written by the
    /// renderer; wheel notches and sticky jumps clamp against it
    /// (reconcile-then-apply, review r5 P2-2). Starts at 0 (review r2
    /// P2-6).
    pub scroll_max: std::cell::Cell<u16>,
    /// The sticky origin line is suppressed after a sticky jump until the
    /// next REAL wheel event (sim jumpToSticky, tui.js:2637-2657: the bar
    /// must never cover the row it just revealed).
    pub sticky_suppressed: bool,
    /// The hit region under the mouse cursor (owner ask, TUI3a item 6).
    /// Value-carrying like clicks: a stale hover can never light up a
    /// different row than the one it was measured on. Render consults it
    /// for hover chrome; palette/menu hover moves the SELECTION instead
    /// (sim onMouseEnter, tui.js:2992/3073).
    pub hovered: Option<Hit>,
    /// The in-app drag selection (owner item 9): set while dragging, kept
    /// after release (the highlight survives the auto-copy), cleared by the
    /// next click or keypress. Screen-space — see [`crate::select`].
    pub selection: Option<crate::select::Selection>,
    /// A left button went down here and has not resolved yet: the potential
    /// selection anchor AND the pending click. On Up with no meaningful
    /// movement the click dispatches from THESE coordinates; a drag that
    /// selected suppresses it (owner item 9's disambiguation law).
    pub mouse_down: Option<(u16, u16)>,
    /// TUI5 item 5 — a left button went down INSIDE the composer text: the
    /// drag (if any) is a COMPOSER selection, never the transcript's
    /// screen-space drag (region disambiguation by drag START). Transient
    /// interaction state; never persisted, never arms anything.
    pub composer_drag: bool,
    pub should_quit: bool,
    /// Set by every state change; cleared when a frame is drawn (rec 6).
    pub dirty: bool,
    /// TUI4d item 14 — the ONE shared animation phase (the sim's CSS
    /// `pulse`/`railShimmer` clocks folded into a single counter). The
    /// runtime advances it every ~600 ms ONLY while [`Self::animated`]
    /// reports a live pulsing element; render derives every pulsing
    /// span's ink from it (even = full ink · odd = the sim's 0.35-opacity
    /// midpoint; `% 3` drives the rail shimmer). Pure render phase:
    /// never persisted, never touching projections or arms.
    pub anim_phase: u8,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            screen: Screen::Boot,
            theme: ThemeKey::Dawn,
            sanctum_tier: SanctumTier::default(),
            projection: SessionProjection::new(),
            identity: IdentityLine::default(),
            composer: crate::composer::Composer::new(),
            drafts: std::collections::HashMap::new(),
            session_title: None,
            session_name: None,
            // The scratch surface's canonical head (the demo script's
            // voice); real sessions claim theirs from the roster.
            session_head: ("Hasan".to_owned(), "(a)".to_owned()),
            msg_queue: Vec::new(),
            queue_mode: false,
            voice: VoiceState::default(),
            listening: false,
            launcher_dir: "~/dev/enterprise-suite".to_owned(),
            session_dir: "~/dev/enterprise-suite".to_owned(),
            card_seq: 0,
            vfs: vfs_seed(),
            launcher_shellout: None,
            chips: Vec::new(),
            view_path: Vec::new(),
            subtree_collapsed: false,
            todos_collapsed: false,
            auto_resuming: false,
            aura: AuraModel::seed(),
            sessions: seed_session_states(),
            active_session: None,
            last_detached: None,
            next_session_id: 4,
            roster: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                crate::script::ROSTER_FIRST_CLAIM,
            )),
            menu_selection: 0,
            palette_selection: 0,
            palette_scroll: 0,
            palette_dismissed: false,
            help_open: false,
            flash: None,
            outbox: Vec::new(),
            requests: Vec::new(),
            turn_active: false,
            scroll_back: std::cell::Cell::new(0),
            scroll_max: std::cell::Cell::new(0),
            sticky_suppressed: false,
            hovered: None,
            selection: None,
            mouse_down: None,
            composer_drag: false,
            should_quit: false,
            dirty: true,
            anim_phase: 0,
        }
    }
}

impl AppModel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The identity that outbound answers and the auto-title micro-call
    /// carry as their `origin`, and that the driver's consumption gates
    /// check (review r2 P1-1): the ATTACHED session's id, or 0 for the
    /// no-session scratch surface. DERIVED, never stored (review TUI4.1,
    /// Fable D2-1 — the old `session_epoch` field was a hand-maintained
    /// twin of `active_session` with a stale monotonicity doc). Identities
    /// themselves never recur: `next_session_id` is monotonic for the
    /// process lifetime (the sim's `s-${Date.now()}` law), so an id-keyed
    /// callback can never find a replacement wearing an old id.
    #[must_use]
    pub fn session_identity(&self) -> u64 {
        self.active_session.unwrap_or(0)
    }

    /// The composer surface currently on screen (TUI5 item 9). Boot maps
    /// to the launcher key: its composer is swallowed by the boot guard,
    /// and the launcher is what boot becomes.
    #[must_use]
    pub fn surface_key(&self) -> DraftKey {
        match self.screen {
            Screen::Aura => DraftKey::Aura,
            _ => self
                .active_session
                .map_or(DraftKey::Launcher, DraftKey::Session),
        }
    }

    /// Park the live composer under the CURRENT surface's key. Callers
    /// pair this with [`Self::restore_draft`] around a surface change —
    /// exactly one stash/restore per transition (a double stash would park
    /// an already-empty composer over the real draft).
    fn stash_draft(&mut self) {
        let key = self.surface_key();
        let draft = std::mem::take(&mut self.composer);
        self.drafts.insert(key, draft);
    }

    /// Bring the NEW surface's parked composer live (empty for a surface
    /// never visited — a fresh session starts with a fresh draft).
    fn restore_draft(&mut self) {
        let key = self.surface_key();
        self.composer = self.drafts.remove(&key).unwrap_or_default();
    }

    /// Flip to the session screen WITH the item-9 draft swap when the
    /// surface key would change (review P1-2: the UserMessage envelope
    /// flip from the AURA screen crossed keys without a swap, leaking the
    /// aura draft onto the session surface and misfiling parked drafts on
    /// the next stash). Same-key flips (launcher scratch, subagent) swap
    /// nothing — an empty round-trip would be harmless but this keeps the
    /// one-stash-one-restore discipline literal.
    fn goto_session_screen(&mut self) {
        let from = self.surface_key();
        let to = self
            .active_session
            .map_or(DraftKey::Launcher, DraftKey::Session);
        if from == to {
            self.screen = Screen::Session;
            return;
        }
        self.stash_draft();
        self.screen = Screen::Session;
        self.restore_draft();
    }

    /// The session's display name — the slug (sim `session.name`), never
    /// the blurb (that lives in the `· session titled` note).
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.session_name.as_deref().unwrap_or("session")
    }

    /// The terminal window title for the current screen (OSC 2).
    #[must_use]
    pub fn window_title(&self) -> String {
        match self.screen {
            Screen::Boot => "haider — starting".to_owned(),
            Screen::Launcher => "haider — launcher".to_owned(),
            Screen::Session | Screen::Subagent | Screen::Aura => {
                // Strip control characters: user text must never smuggle
                // escape sequences into OSC 2 (review r1 P1).
                let title: String = self
                    .display_name()
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect();
                let suffix = if self.screen == Screen::Aura {
                    " · aura"
                } else {
                    ""
                };
                format!("haider — {title} · {}{suffix}", self.identity.device)
            }
        }
    }

    /// The chip the subagent screen is viewing.
    #[must_use]
    pub fn viewed_chip(&self) -> Option<&ChipModel> {
        self.view_path
            .last()
            .and_then(|agent| find_chip(&self.chips, agent))
    }

    /// The status-bar badge with the DERIVED `◔ WAITING · N subagent(s)`
    /// overlay (§2.6): an idle session with live chips waits — display
    /// only, never a synthesized envelope. Interrupted idle (`⏸ IDLE (i)`)
    /// is respected, not overwritten.
    #[must_use]
    pub fn status_badge(&self) -> (String, crate::projection::BadgeTone) {
        let badge = self.projection.badge();
        if badge == "IDLE" {
            let live = tree_live_count(&self.chips);
            if live > 0 {
                let plural = if live > 1 { "s" } else { "" };
                return (
                    format!("◔ WAITING · {live} subagent{plural}"),
                    crate::projection::BadgeTone::Restful,
                );
            }
        }
        (badge, self.projection.badge_tone())
    }

    /// TUI4d item 14 — TRUE while ANY pulsing element is on screen: the
    /// runtime's shared phase clock ticks only then (the efficiency law
    /// this port was once deferred over — ZERO wakeups otherwise; the
    /// dirty-flag economy stays intact). One arm per sim keyframes site
    /// (tui.js:3943-5563); a new animated state must register HERE or it
    /// never moves.
    ///
    /// STATE-based, not viewport-based: a pulsing element shed by a tiny
    /// frame still ticks the clock — the frame then diffs to nothing and
    /// the cost is one bounded render per phase (the CSS analogue: the
    /// sim's animations run whether or not the element is scrolled into
    /// view). Tracking visibility would couple the model to layout.
    #[must_use]
    pub fn animated(&self) -> bool {
        // The status badge's pulse set (WAITING / STARTING / PERMISSION /
        // EFFECT_UNKNOWN, tui.js:5558-5563) — the bar shows on every
        // screen, the derived ◔ WAITING included.
        if crate::projection::badge_pulses(&self.status_badge().0) {
            return true;
        }
        // The ◉ talk chip's live hold (sim `.mic.live`, tui.js:5484-5489).
        if self.listening {
            return true;
        }
        match self.screen {
            // Boot: the gold `.sub` line pulses for the whole starting
            // beat (tui.js:5104-5108).
            Screen::Boot => true,
            // Launcher: a busy row's ◉ dot pulse + rail shimmer
            // (tui.js:4386-4394).
            Screen::Launcher => self.sessions.iter().any(crate::session::SessionState::busy),
            Screen::Session | Screen::Subagent => {
                // `● thinking…` (tui.js:4458-4462) · the ⚒ running tool
                // glyph (tui.js:4524-4530) · the processing todo's box
                // (tui.js:4694-4697) · chip glyph pulses (tui.js:4823-4834)
                // — plus the viewed chip's own thinking tail and tool rows
                // on the subagent screen.
                self.projection.is_thinking()
                    || streaming_tool_live(self.projection.entries())
                    || self
                        .projection
                        .todos()
                        .is_some_and(|panel| panel.pinned && panel.current().is_some())
                    || chips_animated(&self.chips)
                    || (self.screen == Screen::Subagent
                        && self.viewed_chip().is_some_and(|chip| {
                            chip.state == ChipDisplayState::Thinking
                                || streaming_tool_live(chip.transcript.entries())
                        }))
            }
            // Aura: running roster rows (tui.js:4128-4131) + its live
            // hold-to-talk.
            Screen::Aura => {
                self.aura.state == AuraState::Listening
                    || self
                        .aura
                        .roster
                        .iter()
                        .any(|row| row.state == ChipDisplayState::Running)
            }
        }
    }

    /// The palette is open while the composer is a single-line slash query,
    /// esc has not dismissed it (sim `menuDismissed`), and no blocking menu
    /// owns the input. A newline closes it (sim getSuggestions bails on
    /// `\n`, tui.js:235).
    #[must_use]
    pub fn palette_open(&self) -> bool {
        if !self.composer.text().starts_with('/')
            || self.composer.text().contains('\n')
            || self.palette_dismissed
            || self.help_open
        {
            return false;
        }
        // A menu REPLACES the composer, palette included — the session's
        // card on the session screen, the chip's question in its view.
        if self.screen == Screen::Session && self.projection.open_menu().is_some() {
            return false;
        }
        !(self.screen == Screen::Subagent
            && self
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some()))
    }

    /// Current palette rows (commands, or `/theme`'s argument slot) for
    /// rendering and completion.
    #[must_use]
    pub fn palette_items(&self) -> Vec<PaletteItem> {
        palette_items(
            self.composer.text().trim_start_matches('/'),
            matches!(
                self.screen,
                Screen::Session | Screen::Subagent | Screen::Aura
            ),
        )
    }

    /// The inline ghost completion (sim `ghostFor`, tui.js:265-276): the
    /// remainder of the highlighted palette row beyond the typed fragment,
    /// drawn dim after the cursor with a faint `⇥ tab` tag.
    #[must_use]
    pub fn ghost(&self) -> Option<String> {
        if !self.palette_open() {
            return None;
        }
        let items = self.palette_items();
        let item = items
            .get(self.palette_selection.min(items.len().saturating_sub(1)))
            .copied()?;
        let body = self.composer.text().strip_prefix('/')?;
        match item {
            // Command rows exist only while the body is one unfinished
            // token, so the whole body is the fragment.
            PaletteItem::Cmd(spec) => {
                let rest = spec.name.strip_prefix(body)?;
                (!rest.is_empty()).then(|| rest.to_owned())
            }
            PaletteItem::Arg { cmd, value, .. } => {
                if body.ends_with(char::is_whitespace) {
                    return Some((*value).to_owned());
                }
                // Lead case (sim `sugg.lead`): the command is fully typed
                // with no space yet — ghost the space + argument.
                if body.eq_ignore_ascii_case(cmd) {
                    return Some(format!(" {value}"));
                }
                let fragment = body.split_whitespace().last().unwrap_or("");
                let rest = value.strip_prefix(fragment)?;
                (!rest.is_empty()).then(|| rest.to_owned())
            }
        }
    }

    /// Reduce one event into the model. Returns nothing; render reads state,
    /// the runtime drains [`Self::outbox`] and [`Self::requests`].
    /// `StreamEnded` is a no-op and must NOT dirty the frame (r1 P1).
    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => {
                self.dirty = true;
                self.flash = None;
                // TUI5 item 4 — the selection gates run BEFORE the
                // clear-on-keypress law, or ⌃C/Esc could never see the
                // selection they govern.
                if self.selection_key(&key) {
                    return;
                }
                // A keypress clears a finished selection's highlight
                // (owner item 9's clearing law; clicks clear via Down).
                self.selection = None;
                self.handle_key(key);
            }
            AppEvent::Paste(text) => {
                self.dirty = true;
                // While a blocking menu replaces the composer, paste has no
                // target (r2 P2).
                if self.projection.open_menu().is_some() && self.screen == Screen::Session {
                    return;
                }
                // Sim thresholds measure the RAW clipboard — UTF-16 code
                // units and raw newline count, BEFORE any normalization
                // (tui.js:2298-2317). Big pastes become a pill token; small
                // pastes keep their newlines (multi-line composer).
                let raw_lines = text.split('\n').count();
                // TUI5 item 3: paste INSERTS at the cursor (replacing an
                // active selection, item 4) — both the pill token and the
                // literal small-paste path.
                if raw_lines > 3 || text.encode_utf16().count() > 300 {
                    self.composer
                        .insert_str(&format!("[Pasted {raw_lines} lines] "));
                } else {
                    self.composer
                        .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                }
                // Any composer edit re-opens a dismissed palette (sim
                // `setMenuDismissed(false)` on change).
                self.palette_dismissed = false;
            }
            AppEvent::Envelope(payload) => {
                self.dirty = true;
                self.handle_envelope(&payload);
            }
            AppEvent::StreamEnded => {}
        }
    }

    /// TUI5 item 4 — the two keys that act ON a COMPOSER selection,
    /// consumed before anything else sees them:
    ///
    /// - Esc with an active composer selection clears it and NOTHING
    ///   else — "Esc clears selection before any other Esc meaning
    ///   fires" (brief law; the next Esc interrupts/navigates as before).
    ///   Native inputs and Claude Code both deselect-only.
    /// - ⌃C with an active composer selection copies it (the reducer
    ///   holds the exact text → [`AppRequest::CopyText`]) and clears it.
    ///   With NO composer selection ⌃C keeps its TUI4 meaning
    ///   (navigate/quit) exactly — the gate is selection-presence,
    ///   nothing else.
    ///
    /// The gate is scoped to the COMPOSER selection only (review P2-3): a
    /// transcript drag already auto-copied on release, its highlight
    /// clears under the TUI4 any-keypress law, and time-sensitive Esc
    /// (interrupt) / ⌃C (navigate) meanings must not spend a press on a
    /// leftover highlight.
    fn selection_key(&mut self, key: &KeyEvent) -> bool {
        if !self.composer.has_selection() {
            return false;
        }
        if key.code == KeyCode::Esc {
            self.composer.clear_selection();
            return true;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(text) = self.composer.selected_text() {
                self.requests.push(AppRequest::CopyText(text.to_owned()));
            }
            self.composer.clear_selection();
            return true;
        }
        false
    }

    /// TUI5 item 5 — left button DOWN on a composer text row: place the
    /// caret at the clicked boundary and arm the composer-drag mode (the
    /// region-disambiguation law: a drag STARTING here is a composer
    /// selection). `start` and `content` are the hit's render-time values
    /// (the value-carrying law); `col` is the clicked display column
    /// within `content`.
    pub fn composer_press(&mut self, start: usize, content: &str, col: usize) {
        // Value-carrying guard: a one-frame-stale hit whose window no
        // longer exists in the CURRENT text drops the press.
        if start > self.composer.text().len() {
            return;
        }
        let byte = start + crate::composer::byte_at_col(content, col);
        self.composer.press_at(byte);
        self.composer_drag = true;
        self.dirty = true;
    }

    /// Drag with the button held after a composer press: the caret (the
    /// selection's active end) follows the pointer.
    pub fn composer_drag_to(&mut self, byte: usize) {
        if !self.composer_drag {
            return;
        }
        self.composer.drag_to(byte);
        self.dirty = true;
    }

    /// Button UP after a composer press: a selection auto-copies (same
    /// flash as the transcript drag, item 5) and KEEPS its highlight; a
    /// plain click already placed the caret on Down.
    pub fn composer_release(&mut self) {
        if !self.composer_drag {
            return;
        }
        self.composer_drag = false;
        if let Some(text) = self.composer.selected_text() {
            self.requests.push(AppRequest::CopyText(text.to_owned()));
        }
        self.dirty = true;
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // ⌃C is NAVIGATION (owner item 10): from any non-launcher
                // surface it walks back to the launcher — the ← main chip's
                // teardown, nothing more. It never interrupts: a running
                // turn and live chips keep their lifecycle laws (esc owns
                // interrupt, tui.js:2533-2539). From the launcher — and
                // from boot, which has no launcher to return to — it quits,
                // as before. TUI5 item 4: with an ACTIVE composer selection
                // ⌃C COPIES instead (the gate lives in `handle`, before
                // this arm ever sees the key).
                KeyCode::Char('c') => match self.screen {
                    Screen::Launcher | Screen::Boot => self.should_quit = true,
                    _ => self.back_to_launcher(),
                },
                // Ctrl+T cycles the theme (demo stand-in for /theme).
                KeyCode::Char('t') => self.cycle_theme(),
                // ⌃G = the token panel (sim binding) — same honest stub.
                KeyCode::Char('g') => {
                    self.flash =
                        Some("· /tokens — UI ready; lands with the daemon wave (W3)".to_owned());
                }
                // TUI5 items 2+3 — readline editing keys, Claude Code
                // parity: ⌃A/⌃E line edges, ⌃W word-back, ⌃K kill-to-end,
                // ⌃U kill-to-start. Only while the composer actually owns
                // the input (never boot / help / a blocking menu).
                KeyCode::Char('a') if self.composer_owns_input() => {
                    self.composer.line_home(false);
                }
                KeyCode::Char('e') if self.composer_owns_input() => {
                    self.composer.line_end_key(false);
                }
                KeyCode::Char('w') if self.composer_owns_input() => {
                    self.composer.word_backspace();
                    self.note_composer_edit();
                }
                KeyCode::Char('k') if self.composer_owns_input() => {
                    self.composer.kill_to_line_end();
                    self.note_composer_edit();
                }
                KeyCode::Char('u') if self.composer_owns_input() => {
                    self.composer.kill_to_line_start();
                    self.note_composer_edit();
                }
                _ => {}
            }
            return;
        }
        // Boot renders no composer — hidden input must not accumulate or
        // start turns (review r1 P2).
        if self.screen == Screen::Boot {
            return;
        }
        if self.help_open {
            // esc/enter/q close help; everything else is swallowed.
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.help_open = false;
            }
            return;
        }
        // Subagent view (§2.10): esc ALWAYS walks back to the session (the
        // parent is not blocked); the chip's question menu replaces the
        // chip view's composer.
        if self.screen == Screen::Subagent {
            if key.code == KeyCode::Esc {
                self.screen = Screen::Session;
                return;
            }
            if self
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some())
            {
                self.handle_chip_menu_key(key.code);
                return;
            }
        }
        // Aura (§3.1): esc exits to the session if one is attached, else
        // the launcher; exiting never resets aura state.
        if self.screen == Screen::Aura && key.code == KeyCode::Esc {
            self.exit_aura();
            return;
        }
        // A blocking menu REPLACES the composer (sim §3 law).
        if self.projection.open_menu().is_some() && self.screen == Screen::Session {
            self.handle_menu_key(key.code);
            return;
        }
        // ⇧⏎ (kitty-protocol terminals report SHIFT) / ⌥⏎ (the universal
        // path) insert a newline (sim Shift+Enter, tui.js:2792-2796). Must
        // precede the palette branch — a newline also closes the palette.
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            // TUI5 item 3: the newline INSERTS at the cursor like any edit.
            self.composer.insert_str("\n");
            self.palette_dismissed = false;
            return;
        }
        if self.palette_open() {
            match key.code {
                KeyCode::Up => {
                    // Selection wraps over the FULL match list; the window
                    // follows (sim, tui.js:2763-2772 + 2710-2718).
                    let count = self.palette_items().len();
                    if count > 0 {
                        self.palette_selection =
                            (self.palette_selection.min(count - 1) + count - 1) % count;
                        self.scroll_palette_into_view(count);
                    }
                    return;
                }
                KeyCode::Down => {
                    let count = self.palette_items().len();
                    if count > 0 {
                        self.palette_selection = (self.palette_selection + 1) % count;
                        self.scroll_palette_into_view(count);
                    }
                    return;
                }
                KeyCode::Tab => {
                    // Sim acceptSuggestion(tab): arg commands open their
                    // slot; arg-less commands complete in place; an arg row
                    // completes the full command for ⏎ to run.
                    let items = self.palette_items();
                    match items
                        .get(self.palette_selection.min(items.len().saturating_sub(1)))
                        .copied()
                    {
                        Some(PaletteItem::Cmd(spec)) => {
                            self.composer.set_text(if has_arg_slots(spec.name) {
                                format!("/{} ", spec.name)
                            } else {
                                format!("/{}", spec.name)
                            });
                        }
                        Some(PaletteItem::Arg { cmd, value, .. }) => {
                            self.composer.set_text(format!("/{cmd} {value}"));
                        }
                        None => {}
                    }
                    self.palette_selection = 0;
                    self.palette_scroll = 0;
                    return;
                }
                KeyCode::Esc => {
                    // Sim: esc DISMISSES the palette but keeps the typed
                    // text; the next composer edit re-opens it.
                    self.palette_dismissed = true;
                    self.palette_selection = 0;
                    self.palette_scroll = 0;
                    return;
                }
                KeyCode::Enter => {
                    // Enter activates the HIGHLIGHTED row (sim
                    // acceptSuggestion): arg commands enter their slot,
                    // everything else runs.
                    let items = self.palette_items();
                    match items
                        .get(self.palette_selection.min(items.len().saturating_sub(1)))
                        .copied()
                    {
                        Some(item) => self.activate_palette_item(item),
                        None => self.execute_slash(),
                    }
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc if self.screen == Screen::Session => {
                if self.turn_active {
                    // Esc mid-turn INTERRUPTS (sim, tui.js:2533-2539 +
                    // 1551-1567): the script stops, run → cancelled, badge
                    // ⏸ IDLE (i), a transcript note lands — and the session
                    // stays on screen. Only an idle esc walks back. The
                    // held queue drops with the turn (sim tui.js:1557).
                    self.turn_active = false;
                    self.listening = false;
                    self.msg_queue.clear();
                    self.requests.push(AppRequest::Interrupt);
                    self.projection
                        .apply(&EventPayload::RunState(RunState::Cancelled));
                    self.projection
                        .push_note("· interrupted — run → cancelled · idle (i)".to_owned());
                } else {
                    self.back_to_launcher();
                }
            }
            KeyCode::Enter => self.submit_composer(),
            KeyCode::Backspace => {
                // TUI5 item 3: ⌫ deletes the grapheme BEFORE the cursor
                // (or the active selection); ⌥⌫ deletes the word before
                // (ESC-⌫ / kitty ALT — Claude Code binds both).
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.composer.word_backspace();
                } else {
                    self.composer.backspace();
                }
                self.note_composer_edit();
            }
            // Delete (fn⌫ / kDEL, CSI 3~): the grapheme AFTER the cursor.
            KeyCode::Delete => {
                self.composer.delete_forward();
                self.note_composer_edit();
            }
            // TUI5 item 2 — cursor movement. ⇧ extends a selection
            // (item 4); ⌥ moves by word (mac law; iTerm CSI 1;3D). The
            // palette branch above already owns ↑/↓/Tab/⏎ while open, so
            // these arms never fight it.
            KeyCode::Left => {
                let extend = key.modifiers.contains(KeyModifiers::SHIFT);
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.composer.word_left(extend);
                } else {
                    self.composer.move_left(extend);
                }
            }
            KeyCode::Right => {
                let extend = key.modifiers.contains(KeyModifiers::SHIFT);
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.composer.word_right(extend);
                } else {
                    self.composer.move_right(extend);
                }
            }
            // ↑/↓ walk the composer's rows column-sticky (item 2). At the
            // buffer's edge rows they page the input HISTORY instead
            // (item 6, Claude Code behavior) — only with no selection and
            // no ⇧ (a ⇧↑ at the top edge is a selection gesture, not a
            // recall).
            KeyCode::Up if self.composer_owns_input() => {
                let extend = key.modifiers.contains(KeyModifiers::SHIFT);
                if !self.composer.line_up(extend)
                    && !extend
                    && !self.composer.has_selection()
                    && self.composer.history_prev()
                {
                    self.note_composer_edit();
                }
            }
            KeyCode::Down if self.composer_owns_input() => {
                let extend = key.modifiers.contains(KeyModifiers::SHIFT);
                if !self.composer.line_down(extend)
                    && !extend
                    && !self.composer.has_selection()
                    && self.composer.history_next()
                {
                    self.note_composer_edit();
                }
            }
            KeyCode::Home => {
                self.composer
                    .line_home(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::End => {
                self.composer
                    .line_end_key(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::Char(c @ '1'..='3')
                if self.screen == Screen::Launcher && self.composer.is_empty() =>
            {
                let index = (c as usize) - ('1' as usize);
                self.attach_sample(index);
            }
            // ⌥b/⌥f word movement (readline ESC-b/ESC-f — what most mac
            // terminals actually SEND for Option+arrow; Claude Code
            // honors both encodings, so we do too).
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.composer.word_left(false);
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.composer.word_right(false);
            }
            KeyCode::Char(c) => {
                // TUI5 item 3: typing INSERTS at the cursor (never
                // appends) and REPLACES an active selection (item 4).
                self.composer.insert_str(c.encode_utf8(&mut [0u8; 4]));
                self.note_composer_edit();
                // Typing decays interrupted-idle → idle (sim, tui.js:3020).
                if self.projection.interrupted() {
                    self.projection.apply(&EventPayload::IdleDecayed);
                }
            }
            _ => {}
        }
    }

    /// The composer is the live input target: no boot screen, no help
    /// overlay, no blocking menu owning the keys (session card or the
    /// viewed chip's question). Gates the TUI5 editing keys so ⌃K on a
    /// menu can never eat a hidden draft.
    #[must_use]
    fn composer_owns_input(&self) -> bool {
        if self.screen == Screen::Boot || self.help_open {
            return false;
        }
        if self.screen == Screen::Session && self.projection.open_menu().is_some() {
            return false;
        }
        !(self.screen == Screen::Subagent
            && self
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some()))
    }

    /// The composer-edit epilogue every text-changing key shares (the sim
    /// resets suggestion state on any change): palette selection/scroll
    /// reset + a dismissed palette re-opens.
    fn note_composer_edit(&mut self) {
        self.palette_selection = 0;
        self.palette_scroll = 0;
        self.palette_dismissed = false;
    }

    /// Sim submit() preprocessing, exact order (tui.js:1966-2041 — the
    /// aura/subagent screen steps land with their screens; the boot-queue
    /// step is unreachable here because the boot screen swallows input by
    /// earlier review law r1 P2).
    fn submit_composer(&mut self) {
        // TUI5: the take records the submitted text in this surface's
        // input ring (item 6) and clears cursor/selection state (item 8).
        // Slash submits take SILENTLY — execute_slash records the
        // canonical form (review P3-9, one entry per invocation).
        let is_slash = self.composer.text().trim().starts_with('/');
        let text = if is_slash {
            self.composer.take_silent()
        } else {
            self.composer.take_for_submit()
        }
        .trim()
        .to_owned();
        self.palette_selection = 0;
        self.palette_scroll = 0;
        self.palette_dismissed = false;
        if text.is_empty() {
            // Empty ⏎ on the launcher re-attaches the most recently left
            // session (a port law; the detach model keeps it honest by id).
            if self.screen == Screen::Launcher
                && let Some(id) = self.last_detached
            {
                self.open_session(id);
            }
            return;
        }
        if text.starts_with('/') {
            self.composer.set_text(text);
            self.execute_slash();
            return;
        }
        // §4 step 3: on the aura screen non-slash text drives orchestrate
        // ONLY while the aura is idle (otherwise silently dropped).
        if self.screen == Screen::Aura {
            if self.aura.state == AuraState::Idle {
                self.aura_submit(text, false);
            }
            return;
        }
        // Shell builtins run against the VFS — local, instant, NO model
        // turn (sim tui.js:1993-2008) — never on the subagent screen, and
        // they never start a session.
        let first_word = text.split_whitespace().next().unwrap_or("");
        if self.screen != Screen::Subagent
            && SHELL_CMDS.contains(&first_word.to_ascii_lowercase().as_str())
        {
            self.run_shell_line(&text);
            return;
        }
        // §4 step 6: the subagent screen steers ITS chip (respondChip).
        if self.screen == Screen::Subagent {
            if let Some(agent) = self.view_path.last().cloned() {
                self.requests.push(AppRequest::ChipSubmit { agent, text });
            }
            return;
        }
        // Mid-turn input (sim tui.js:2027-2038): queue mode holds it for
        // turn end (⧗ panel, consumed with no idle); steer delivers the
        // row now with the sim's note (display-only — the running script
        // is not altered, same as the sim).
        if self.screen == Screen::Session && self.turn_active {
            if self.queue_mode {
                self.msg_queue.push(text);
            } else {
                self.projection.apply(&EventPayload::UserMessage {
                    text,
                    attachments: vec![],
                    mode: DeliveryMode::Steer,
                });
                self.projection.push_note(
                    "· steered — delivered at the next safe boundary of the current turn"
                        .to_owned(),
                );
            }
            return;
        }
        // Typing on the LAUNCHER starts a FRESH session (sim promise,
        // tui.js:2013-2016 `newSession`) — the one left behind keeps
        // running and shows as busy in its launcher row.
        if self.screen == Screen::Launcher {
            self.new_session(&text);
        }
        // The blurb is NOT set here: the sim's micro-call names the session
        // inside its own 1.5 s callback. The callback SURVIVES an interrupt
        // (bare setTimeout in the sim) — only a session replacement voids it,
        // via the origin identity (review r2 P2-6).
        let title = self.session_title.is_none();
        self.screen = Screen::Session;
        self.turn_active = true;
        self.scroll_back.set(0);
        self.requests.push(AppRequest::SubmitText {
            text,
            voice: false,
            title,
        });
    }

    /// One shell-builtin line against the VFS: a session gets a transcript
    /// `$` row; the launcher gets its `.shellout` block (sim tui.js:3302).
    fn run_shell_line(&mut self, line: &str) {
        let in_session = self.screen == Screen::Session;
        let cwd = if in_session {
            self.session_dir.clone()
        } else {
            self.launcher_dir.clone()
        };
        let (out, retarget) = run_shell(line, &cwd, &mut self.vfs);
        if let Some(dir) = retarget {
            if in_session {
                self.session_dir = dir;
            } else {
                self.launcher_dir = dir;
            }
        }
        if in_session {
            self.projection.push_shell(line.to_owned(), out);
        } else {
            self.launcher_shellout = Some((line.to_owned(), out));
        }
    }

    /// A voice submission (sim /say + push-to-talk, tui.js:1865-1875):
    /// ◉ user row + `◉ heard` note ride the reducer; the script skips its
    /// own UserMessage and tags streamed rows `♪ speaking`.
    fn submit_voice(&mut self, text: String) {
        if self.screen == Screen::Launcher {
            self.new_session(&text);
        }
        self.projection.push_user_voice(text.clone());
        self.projection
            .push_note(format!("◉ heard · {}", self.voice.stt));
        let title = self.session_title.is_none();
        self.goto_session_screen(); // review P1-2: draft-aware flip
        self.turn_active = true;
        self.scroll_back.set(0);
        self.requests.push(AppRequest::SubmitText {
            text,
            voice: true,
            title,
        });
    }

    /// The ◉ talk hold finished (driver timer): submit the canned phrase
    /// through the voice path (sim tui.js:2044-2054).
    pub fn talk_fire(&mut self) {
        // A hold nobody is holding fires nothing: Esc (and any navigation)
        // clears `listening`, so the 1.3 s timer can no longer land on the
        // Launcher and yank the user into a fresh canned session
        // (review P1-3). The timer is ALSO session-owned, so a fresh
        // session cancels it outright.
        if !self.listening {
            return;
        }
        self.listening = false;
        self.dirty = true;
        // Sim `speak` requires an attached, idle session (tui.js:2045);
        // the launcher mic is inert, so the hold can never fabricate one.
        if self.turn_active || !self.voice.enabled || self.screen != Screen::Session {
            return;
        }
        self.submit_voice(TALK_PHRASE.to_owned());
    }

    /// An aura orchestrate turn: user row + driver request (§3.4).
    fn aura_submit(&mut self, text: String, voice: bool) {
        if voice {
            self.aura.transcript.push_user_voice(text.clone());
        } else {
            self.aura.transcript.apply(&EventPayload::UserMessage {
                text: text.clone(),
                attachments: vec![],
                mode: DeliveryMode::Steer,
            });
        }
        // The orb leaves idle NOW, not when the first async beat lands:
        // the `idle` submit gate is what stops two rapid submits from
        // interleaving (review P1-2; the driver additionally cancels the
        // previous run, as the sim's `++auraRunRef` does).
        self.aura.state = AuraState::Thinking;
        self.aura.runs += 1;
        self.requests.push(AppRequest::AuraSubmit { text, voice });
    }

    /// The aura talk hold finished (driver timer, tui.js:2128-2132).
    pub fn aura_talk_fire(&mut self) {
        // Only a hold still in `listening` fires (navigation away cancels
        // the arm; a run started meanwhile owns the orb).
        if self.aura.state != AuraState::Listening {
            return;
        }
        self.dirty = true;
        self.aura_submit(crate::script::AURA_TALK_PHRASE.to_owned(), true);
    }

    /// Enter the aura stage (the `/aura` command and the launcher's Aura
    /// row share this): the departing surface's draft parks, the aura's
    /// own comes live (TUI5 item 9 — Aura has its own composer instance).
    fn enter_aura(&mut self) {
        if self.screen == Screen::Aura {
            return;
        }
        self.stash_draft();
        self.screen = Screen::Aura;
        self.restore_draft();
    }

    /// Esc from the aura stage: back to the session if one is attached,
    /// else the launcher — aura state persists either way.
    fn exit_aura(&mut self) {
        // TUI5 item 9: the aura's draft parks under its own key; the
        // return surface's draft comes live below.
        self.stash_draft();
        // TUI4c: attachment is the map's word now — a checked-out session
        // (or a content-bearing scratch) takes esc back to the session;
        // an aura entered from the menu returns to the menu.
        self.screen = if self.active_session.is_some()
            || !self.projection.entries().is_empty()
            || self.session_name.is_some()
        {
            Screen::Session
        } else {
            Screen::Launcher
        };
        self.restore_draft();
    }

    /// Chip close lifecycle flags (§2.5) — the DRIVER owns the 5 s removal
    /// timer and the resume check; returns whether the chip WAS live
    /// (closing the last live child discharges the wait).
    pub fn close_chip_state(&mut self, agent: &str) -> Option<bool> {
        let was_live = close_chip_core(&mut self.chips, &mut self.projection, agent)?;
        // Sim closeChip (tui.js:1176-1178): the screen ALWAYS returns to the
        // session, but the remembered view path only clears when the CLOSED
        // chip is the one being viewed (`viewChipId === chipId ? null : v`).
        self.screen = Screen::Session;
        if self.view_path.last().is_some_and(|last| last == agent) {
            self.view_path.clear();
        }
        self.dirty = true;
        Some(was_live)
    }

    /// Keys while the viewed chip's question menu replaces its composer
    /// (§2.10): digits/arrows/enter answer; the parent is never blocked.
    fn handle_chip_menu_key(&mut self, code: KeyCode) {
        let Some(menu) = self
            .viewed_chip()
            .and_then(ChipModel::question_menu)
            .cloned()
        else {
            return;
        };
        let option_count = menu.options.len();
        match code {
            KeyCode::Up if option_count > 0 => {
                self.menu_selection =
                    (self.menu_selection.min(option_count - 1) + option_count - 1) % option_count;
            }
            KeyCode::Down if option_count > 0 => {
                self.menu_selection = (self.menu_selection + 1) % option_count;
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < option_count {
                    self.menu_selection = index;
                    self.answer_chip_menu(&menu);
                }
            }
            KeyCode::Enter => self.answer_chip_menu(&menu),
            _ => {}
        }
    }

    fn answer_chip_menu(&mut self, menu: &Menu) {
        let Some(option) = menu.options.get(self.menu_selection) else {
            return;
        };
        self.outbox.push(OutboundAnswer {
            origin: self.session_identity(),
            answer: MenuAnswer {
                menu: menu.id.clone(),
                option_key: Some(option.key.clone()),
                option_index: u32::try_from(self.menu_selection).unwrap_or(u32::MAX),
                value: None,
                via: AnswerVia::Tui,
            },
        });
        self.menu_selection = 0;
    }

    /// Keep the palette selection inside the visible window (sim CmdMenu
    /// scroll keep-visible, tui.js:2710-2718).
    fn scroll_palette_into_view(&mut self, count: usize) {
        self.palette_scroll = self
            .palette_scroll
            .min(count.saturating_sub(PALETTE_MAX_ROWS));
        if self.palette_selection < self.palette_scroll {
            self.palette_scroll = self.palette_selection;
        } else if self.palette_selection >= self.palette_scroll + PALETTE_MAX_ROWS {
            self.palette_scroll = self.palette_selection + 1 - PALETTE_MAX_ROWS;
        }
    }

    /// Activate one palette row — ⏎ and mouse click share this law (the
    /// click carries the VALUE, so a stale map can never run a different
    /// row). Sim acceptSuggestion (tui.js:2720-2753): a command with
    /// argument slots ENTERS its slot instead of executing; arg-less
    /// commands and argument rows execute.
    fn activate_palette_item(&mut self, item: PaletteItem) {
        match item {
            PaletteItem::Cmd(spec) if has_arg_slots(spec.name) => {
                self.composer.set_text(format!("/{} ", spec.name));
                self.palette_selection = 0;
                self.palette_scroll = 0;
                self.palette_dismissed = false;
            }
            PaletteItem::Cmd(spec) => {
                let args: String = self
                    .composer
                    .text()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ");
                self.composer.set_text(if args.is_empty() {
                    format!("/{}", spec.name)
                } else {
                    format!("/{} {args}", spec.name)
                });
                self.execute_slash();
            }
            PaletteItem::Arg { cmd, value, .. } => {
                self.composer.set_text(format!("/{cmd} {value}"));
                self.execute_slash();
            }
        }
    }

    fn execute_slash(&mut self) {
        let raw = self
            .composer
            .text()
            .trim_start_matches('/')
            .trim()
            .to_owned();
        // TUI5 item 6: slash executions are recallable like any submit —
        // palette-activated commands never pass `take_for_submit`, so the
        // ring is fed here with the CANONICAL form. The consecutive-dupe
        // dedupe absorbs the double record on the plain-⏎ path.
        if !raw.is_empty() {
            self.composer.record_submitted(&format!("/{raw}"));
        }
        self.composer.clear();
        self.palette_selection = 0;
        self.palette_scroll = 0;
        self.palette_dismissed = false;
        let mut words = raw.split_whitespace();
        let name = words.next().unwrap_or("").to_ascii_lowercase();
        let remainder = words.collect::<Vec<_>>().join(" ");
        let arg = remainder
            .split_whitespace()
            .next()
            .map(str::to_ascii_lowercase);
        match name.as_str() {
            "help" => self.help_open = true,
            "theme" => match arg.as_deref() {
                Some(name) => match ThemeKey::parse(name) {
                    Some(key) => {
                        self.theme = key;
                        self.flash = Some(format!("· theme → {}", key.theme().label));
                    }
                    None => {
                        self.flash =
                            Some(format!("· unknown theme “{name}” — dawn · ivory · dark"));
                    }
                },
                None => {
                    // Documented divergence: the sim only LISTS the themes
                    // on a bare /theme (tui.js:1729-1733); in a TUI cycling
                    // is the better default — cycle, name the result, and
                    // still list the choices.
                    self.cycle_theme();
                    if let Some(flash) = &mut self.flash {
                        flash.push_str(" · themes — dawn · ivory · dark");
                    }
                }
            },
            "clear" | "back" => {
                // Sim tui.js:1950-1958: /clear DETACHES (activeId = null)
                // and nothing more — the session keeps running and shows
                // as busy in its row. The /clear fresh-start promise
                // (review r1 P2) is kept by `new_session`: the next typed
                // message starts a brand-new session, never this one.
                self.back_to_launcher();
            }
            "reset" => {
                // TUI5 item 9: park the departing surface first; session
                // drafts die with the reseed (the identity law — a
                // reseeded roster must not wear old drafts) and the
                // aura's dies with its reseed below. The LAUNCHER draft
                // SURVIVES — documented choice: the launcher is not an
                // identity-keyed surface, and the owner's monotonic
                // rules govern session ids only.
                self.stash_draft();
                self.fresh_session();
                self.sessions = seed_session_states();
                self.active_session = None;
                self.last_detached = None;
                // `next_session_id` is deliberately NOT reset (review
                // TUI4.1 P1-2): the control-tagged auto-title callback is
                // keyed by session id and survives /reset by design (sim:
                // a bare setTimeout); resetting the allocator let a
                // replacement session reuse the old id and receive the old
                // title. The sim's `s-${Date.now()}` ids never recur —
                // monotonicity ports that law, killing the whole class.
                self.roster.store(
                    crate::script::ROSTER_FIRST_CLAIM,
                    std::sync::atomic::Ordering::SeqCst,
                );
                self.aura = AuraModel::seed();
                self.requests.push(AppRequest::ResetAura);
                // Sim tui.js:1918: the state file dies with the reset; the
                // seeds re-save on the next change exactly as the sim's
                // save effect refills localStorage after removeItem.
                self.requests.push(AppRequest::PurgeDemoStore);
                self.screen = Screen::Launcher;
                self.drafts.retain(|key, _| *key == DraftKey::Launcher);
                self.restore_draft();
                self.flash = Some("· demo reset".to_owned());
            }
            "quit" | "exit" => self.requests.push(AppRequest::Quit),
            "aura" => self.enter_aura(),
            "compact" => {
                // Manual compaction (sim tui.js:1791-1806). Adapted gate:
                // the sim's single-threaded state writes tolerate /compact
                // mid-turn; the envelope demo refuses honestly instead of
                // clobbering a live turn's run state.
                if self.screen != Screen::Session {
                    self.flash = Some("· /compact — session only".to_owned());
                } else if self.turn_active {
                    self.flash = Some("· /compact — wait for the turn to end".to_owned());
                } else {
                    self.turn_active = true;
                    self.requests.push(AppRequest::Compact);
                }
            }
            "queue" => {
                // Mid-turn input mode (sim tui.js:1810-1817).
                if self.screen != Screen::Session {
                    self.flash = Some("· /queue — session only".to_owned());
                } else {
                    match arg.as_deref() {
                        Some("steer") => {
                            self.queue_mode = false;
                            self.projection.push_note(
                                "· mid-turn input → STEER — delivered at the next safe boundary"
                                    .to_owned(),
                            );
                        }
                        Some("turn" | "queue") => {
                            self.queue_mode = true;
                            self.projection.push_note(
                                "· mid-turn input → QUEUE — held until the turn ends, then consumed without idling"
                                    .to_owned(),
                            );
                        }
                        _ => {
                            let mode = if self.queue_mode {
                                "queue (after turn)"
                            } else {
                                "steer (safe boundary)"
                            };
                            self.projection.push_note(format!(
                                "· mid-turn input mode is {mode} — /queue steer|turn"
                            ));
                        }
                    }
                }
            }
            "say" => {
                // Voice turn via simulated STT (sim tui.js:1865-1875).
                if self.screen != Screen::Session {
                    self.flash = Some("· /say — session only".to_owned());
                } else if !self.voice.enabled {
                    self.projection
                        .push_note("· enable voice first with /voice".to_owned());
                } else if self.turn_active {
                    // Sim-honest: the note promises a queue that never
                    // happens — ported as-is (tui.js:1868).
                    self.projection
                        .push_note("· busy — voice turn queues once idle".to_owned());
                } else if remainder.is_empty() {
                    self.projection
                        .push_note("· /say <words> — what should I hear?".to_owned());
                } else {
                    self.submit_voice(remainder);
                }
            }
            "voice" => {
                if self.screen == Screen::Session {
                    self.card_seq += 1;
                    let card = voice_card(&self.voice, self.card_seq);
                    self.projection.apply(&EventPayload::MenuOpened(card));
                } else {
                    self.flash = Some("· /voice — session only".to_owned());
                }
            }
            "tools" => {
                if self.screen == Screen::Session {
                    self.card_seq += 1;
                    self.projection
                        .apply(&EventPayload::MenuOpened(tools_card(self.card_seq)));
                } else {
                    self.flash = Some("· /tools — session only".to_owned());
                }
            }
            "" => {}
            other => {
                // Known stubs name their wave; typos say so (review r1 P2).
                let wave = match other {
                    "model" | "provider" | "login" | "account" | "accounts" => {
                        Some("the account switchboard (W3)")
                    }
                    "sessions" | "tree" | "fork" | "rename" | "tokens" => {
                        Some("the daemon wave (W3)")
                    }
                    "peers" => Some("the mesh wave (post-v0.1)"),
                    "hooks" | "update" => Some("the gates wave (W4)"),
                    _ => None,
                };
                self.flash = Some(match wave {
                    Some(wave) => format!("· /{other} — UI ready; lands with {wave}"),
                    None => format!("· unknown command /{other} — /help lists commands"),
                });
            }
        }
    }

    fn handle_menu_key(&mut self, code: KeyCode) {
        let Some(menu) = self.projection.open_menu() else {
            return;
        };
        let option_count = menu.options.len();
        // Esc is SWALLOWED for blocking cards (sim menu law); non-blocking
        // command cards (/voice, /tools) dismiss.
        if code == KeyCode::Esc {
            if !menu.blocking {
                let id = menu.id.clone();
                self.projection.apply(&EventPayload::MenuClosed {
                    menu: id,
                    reason: MenuCloseReason::Dismissed,
                });
            }
            return;
        }
        match code {
            // Selection wraps around (sim, tui.js:2441-2449).
            KeyCode::Up if option_count > 0 => {
                self.menu_selection =
                    (self.menu_selection.min(option_count - 1) + option_count - 1) % option_count;
            }
            KeyCode::Down if option_count > 0 => {
                self.menu_selection = (self.menu_selection + 1) % option_count;
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < option_count {
                    self.menu_selection = index;
                    self.submit_menu_answer();
                }
            }
            KeyCode::Enter => self.submit_menu_answer(),
            _ => {}
        }
    }

    fn submit_menu_answer(&mut self) {
        let Some(menu) = self.projection.open_menu() else {
            return;
        };
        let Some(option) = menu.options.get(self.menu_selection) else {
            return;
        };
        let answer = MenuAnswer {
            menu: menu.id.clone(),
            option_key: Some(option.key.clone()),
            option_index: u32::try_from(self.menu_selection).unwrap_or(u32::MAX),
            value: None,
            via: AnswerVia::Tui,
        };
        self.outbox.push(OutboundAnswer {
            origin: self.session_identity(),
            answer,
        });
        self.menu_selection = 0;
    }

    fn handle_envelope(&mut self, payload: &EventPayload) {
        // Screen auto-transitions (sim: boot → launcher when startup
        // completes; the first user message attaches the session view).
        if matches!(payload, EventPayload::HarnessStatus(HarnessStatus::Ready))
            && self.screen == Screen::Boot
        {
            self.screen = Screen::Launcher;
        }
        if let EventPayload::UserMessage { .. } = payload {
            self.goto_session_screen();
            self.turn_active = true;
            // NB: no titling here. The sim names a session ONLY inside the
            // 1.5 s micro-call callback (tui.js:1219-1227); titling on the
            // user-row envelope pre-empted that callback, so its note never
            // landed (review P2-12).
        }
        if let EventPayload::RunState(state) = payload
            && state.is_terminal()
        {
            self.turn_active = false;
            self.auto_resuming = false;
            // The `♪ speaking` tag ends where the TURN ends. A trailing
            // `Voice(false)` beat could not: a branch parked on a menu
            // never reaches its own tail, so later ordinary rows kept
            // rendering as spoken (review P2-10).
            self.projection.set_voice_live(false);
        }
        if matches!(payload, EventPayload::MenuOpened(_)) {
            self.menu_selection = 0;
        }
        self.projection.apply(payload);
        // Chip questions are Subagent-scoped menus living in the CHIP's
        // projection — an answer closes the matching chip card too.
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            fn route(chips: &mut [ChipModel], payload: &EventPayload) {
                for chip in chips {
                    chip.transcript.apply(payload);
                    route(&mut chip.children, payload);
                }
            }
            route(&mut self.chips, payload);
        }
        // Command-card consequences (sim /voice + /tools, tui.js:1824-1906)
        // apply AFTER the answer closed the card.
        if let EventPayload::MenuAnswered(answer) = payload {
            let index = usize::try_from(answer.option_index).unwrap_or(usize::MAX);
            let id = answer.menu.as_str();
            if id.starts_with(VOICE_CARD_PREFIX) {
                self.voice_card_answered(index);
            } else if id.starts_with(TOOLS_CARD_PREFIX) {
                self.tools_card_answered(index);
            }
        }
    }

    /// `/voice` card consequences (sim tui.js:1824-1864).
    fn voice_card_answered(&mut self, index: usize) {
        match index {
            0..=2 => {
                let (stt, tts, duplex) = match index {
                    0 => ("whisper-large-v3", "openai-tts", false),
                    1 => ("deepgram-nova-3", "elevenlabs", false),
                    _ => ("gpt-realtime", "gpt-realtime", true),
                };
                self.voice = VoiceState {
                    enabled: true,
                    stt: stt.to_owned(),
                    tts: tts.to_owned(),
                    duplex,
                };
                let pipeline = if duplex {
                    "gpt-realtime native duplex".to_owned()
                } else {
                    format!("{stt} → {tts}")
                };
                self.projection.push_note(format!(
                    "· voice enabled · {pipeline} · hold-to-talk under the input, or /say <words>"
                ));
            }
            3 => {
                if self.voice.enabled {
                    self.voice.enabled = false;
                    self.projection.push_note("· voice disabled".to_owned());
                } else {
                    self.projection.push_note("· voice stays off".to_owned());
                }
            }
            _ => {}
        }
    }

    /// `/tools` card consequences (sim tui.js:1876-1906).
    fn tools_card_answered(&mut self, index: usize) {
        const MODES: [&str; 3] = [
            "fire-and-forget — the turn continues the instant it dispatches",
            "await — the turn parks in TOOL_RUNNING until the result returns",
            "deferred — returns a ticket, the session waits in WAITING(dependency) for the callback",
        ];
        match index {
            0..=2 => self.projection.push_note(format!(
                "· custom tool registered · dispatch = {}",
                MODES[index]
            )),
            3 => self.projection.push_note("· tools card closed".to_owned()),
            _ => {}
        }
    }

    /// Start-fresh semantics (review r1 P2): a new session begins from an
    /// empty projection; the previous demo transcript does not leak in —
    /// including its scroll ceiling and any pending timers
    /// (ResetAllSessions cancels the Session and Chip ARMS — Aura
    /// deliberately survives, see `ArmOwner` — so a stale idle-decay or
    /// script beat from the OLD session drops at consumption).
    fn fresh_session(&mut self) {
        // Answers and micro-calls born under the old surface are now
        // stale: any that never left the outbox are dropped outright
        // (review r2 P1-1); in-flight ones fail the driver's
        // [`Self::session_identity`] gate — the reset surface has no
        // session, so their by-id origin can never match.
        self.outbox.clear();
        self.projection = SessionProjection::new();
        self.session_title = None;
        self.session_name = None;
        self.turn_active = false;
        self.msg_queue.clear();
        self.queue_mode = false;
        self.voice = VoiceState::default();
        self.listening = false;
        self.session_dir = self.launcher_dir.clone();
        self.chips.clear();
        self.view_path.clear();
        self.subtree_collapsed = false;
        self.todos_collapsed = false;
        self.auto_resuming = false;
        self.scroll_back.set(0);
        self.scroll_max.set(0);
        self.sticky_suppressed = false;
        self.requests.push(AppRequest::ResetAllSessions);
    }

    /// Attach a sample session by NAME (the clicked row's identity, P2-9).
    fn attach_sample_named(&mut self, name: &str) {
        if let Some(id) = self
            .sessions
            .iter()
            .find(|entry| entry.name.as_deref() == Some(name))
            .map(|entry| entry.id)
        {
            self.open_session(id);
        }
    }

    /// Attach the launcher's nth row (digit binding). TUI4c: switching is
    /// FREE — the sim's `openSession` never blocks on a running turn
    /// (tui.js:1606: "attaching never cancels a turn"); the old
    /// one-turn-at-a-time flash guarded a single shared projection that no
    /// longer exists.
    fn attach_sample(&mut self, index: usize) {
        if let Some(id) = self.sessions.get(index).map(|entry| entry.id) {
            self.open_session(id);
        }
    }

    /// Sim `openSession` (tui.js:1606-1615): sweep closed chips whose 5 s
    /// removal never fired, attach, and NOTHING else — no turn starts
    /// (owner item 1), and the one left behind keeps running.
    pub fn open_session(&mut self, id: u64) {
        if self.active_session == Some(id) {
            self.screen = Screen::Session;
            return;
        }
        // TUI5 item 9: park the departing surface's draft BEFORE identity
        // flips (checkin() itself is draft-free — exactly one stash and
        // one restore per transition).
        self.stash_draft();
        self.checkin();
        let Some(index) = self.sessions.iter().position(|entry| entry.id == id) else {
            // Unknown id: the checkin left us on the no-session surface —
            // bring ITS (the launcher's) draft live, stranding nothing.
            self.restore_draft();
            return;
        };
        // Move the slot out so its fields can swap with `self`'s without
        // aliasing; the slot keeps a neutral placeholder meanwhile.
        let mut slot = std::mem::replace(
            &mut self.sessions[index],
            crate::session::SessionState::neutral(id),
        );
        crate::session::sweep_closed_chips(&mut slot.chips);
        self.projection = std::mem::replace(&mut slot.projection, SessionProjection::new());
        self.chips = std::mem::take(&mut slot.chips);
        self.msg_queue = std::mem::take(&mut slot.msg_queue);
        self.queue_mode = slot.queue_mode;
        self.turn_active = slot.turn_active;
        self.auto_resuming = slot.auto_resuming;
        self.subtree_collapsed = slot.subtree_collapsed;
        self.todos_collapsed = slot.todos_collapsed;
        self.session_title = slot.title.take();
        self.session_name = slot.name.take();
        self.session_head = std::mem::take(&mut slot.head);
        self.session_dir = std::mem::take(&mut slot.dir);
        self.sessions[index] = slot;
        self.active_session = Some(id);
        self.menu_selection = 0;
        self.view_path.clear();
        self.screen = Screen::Session;
        self.scroll_back.set(0);
        self.scroll_max.set(0);
        self.sticky_suppressed = false;
        // TUI5 item 9: the attached session's own draft comes live —
        // text, cursor, selection and input ring exactly as it left.
        self.restore_draft();
    }

    /// Detach: write the live fields back into the session's slot (sim
    /// `setActiveId(null)` — the state lives on and its scripts keep
    /// running). The surface then returns to the neutral no-session state
    /// item 12 requires of the launcher.
    pub fn checkin(&mut self) {
        let Some(active) = self.active_session.take() else {
            return;
        };
        if let Some(index) = self.sessions.iter().position(|entry| entry.id == active) {
            let slot = &mut self.sessions[index];
            slot.projection = std::mem::replace(&mut self.projection, SessionProjection::new());
            slot.chips = std::mem::take(&mut self.chips);
            slot.msg_queue = std::mem::take(&mut self.msg_queue);
            slot.queue_mode = std::mem::take(&mut self.queue_mode);
            slot.turn_active = std::mem::take(&mut self.turn_active);
            slot.auto_resuming = std::mem::take(&mut self.auto_resuming);
            slot.subtree_collapsed = std::mem::take(&mut self.subtree_collapsed);
            slot.todos_collapsed = std::mem::take(&mut self.todos_collapsed);
            slot.title = self.session_title.take();
            slot.name = self.session_name.take();
            slot.head = std::mem::replace(
                &mut self.session_head,
                ("Hasan".to_owned(), "(a)".to_owned()),
            );
            slot.dir = std::mem::replace(&mut self.session_dir, self.launcher_dir.clone());
        }
        self.last_detached = Some(active);
        self.msg_queue.clear();
        self.queue_mode = false;
        self.view_path.clear();
        self.menu_selection = 0;
        self.scroll_back.set(0);
        self.scroll_max.set(0);
        self.sticky_suppressed = false;
    }

    /// Sim `newSession` (tui.js:1617-1650): a fresh id, a head claimed
    /// from the roster (the seeds hold 0-2, so the first user session
    /// claims Hasan), the launcher dir, newest-first in the list. The
    /// session left behind is checked in, never cancelled.
    fn new_session(&mut self, text: &str) {
        self.checkin();
        let id = self.next_session_id;
        self.next_session_id += 1;
        let ros = self
            .roster
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let head = crate::script::roster_at(ros);
        let mut entry = crate::session::SessionState::neutral(id);
        entry.name = Some(slug_name(text));
        entry.head = (head.callsign, head.hon.to_owned());
        entry.head_ros = Some(ros);
        entry.dir = self.launcher_dir.clone();
        entry.model_short = self.identity.model_short.clone();
        entry.device = self.identity.device.clone();
        entry.ago = "now".to_owned();
        self.sessions.insert(0, entry);
        self.open_session(id);
        // Review P3-8: the founding message recalls IN the new session
        // (Claude Code recalls in-conversation); the launcher's own ring
        // kept its copy via take_for_submit before the surface swap.
        self.composer.record_submitted(text);
    }

    /// Walk back to the launcher — the ONE teardown every back path shares
    /// (the ← main chip, idle esc, ⌃C navigation per owner item 10): the
    /// live talk hold is cancelled (P1-3), the subagent view path and any
    /// overlay reset. NAVIGATION ONLY — the projection, chips, queue and a
    /// running turn are untouched, so the session resumes exactly where it
    /// was left.
    pub fn back_to_launcher(&mut self) {
        // TUI5 item 9: park the departing surface's draft (session, aura,
        // or the scratch surface — which shares the launcher key, so its
        // stash/restore is an exact round-trip).
        self.stash_draft();
        // TUI4c: leaving DETACHES (sim `setActiveId(null)`, tui.js:1956) —
        // the session's state checks into its slot, its scripts keep
        // running, and the launcher's surface derives from NO session
        // (item 12: a background turn never reaches the main menu's badge).
        if self.active_session.is_some() {
            self.checkin();
        } else if !self.projection.entries().is_empty() || self.turn_active {
            // A content-bearing SCRATCH (envelope-driven flows with no
            // session id — the headless harness, the plain oracle): there
            // is no slot to keep it in, so /clear's fresh-start promise
            // applies literally — reset and stop its scripts. Real UI
            // flows always mint a session id first (`new_session`).
            self.fresh_session();
        }
        self.screen = Screen::Launcher;
        self.listening = false;
        self.view_path.clear();
        self.help_open = false;
        // TUI5 item 9: the launcher's own draft comes back.
        self.restore_draft();
    }

    /// A NON-attached session's slot (background event routing).
    pub fn session_entry_mut(&mut self, id: u64) -> Option<&mut crate::session::SessionState> {
        if self.active_session == Some(id) {
            return None;
        }
        self.sessions.iter_mut().find(|entry| entry.id == id)
    }

    /// A left-click resolved through the frame's hit map. The map may be
    /// one frame stale (review r2 P2-2): hits carry values and every
    /// context-sensitive hit re-checks its context — activate exactly what
    /// was clicked, or drop the click.
    pub fn handle_hit(&mut self, hit: Hit) {
        self.dirty = true;
        self.flash = None;
        // A visible overlay owns the screen; hits from the covered frame
        // must not act through it.
        if self.help_open {
            return;
        }
        match hit {
            // Every hit below re-checks its OWNING SURFACE: the map may be
            // one frame stale, so a rect from a screen we have since left
            // must never act (review P1-5 — the law documented above was
            // only honored by the palette/menu hits).
            Hit::AttachSample(name) if self.screen == Screen::Launcher => {
                self.attach_sample_named(&name);
            }
            Hit::ExtraRow(which) if self.screen == Screen::Launcher => match which {
                LauncherRow::Aura => self.enter_aura(),
                LauncherRow::Accounts => {
                    self.flash = Some(
                        "· /accounts — UI ready; lands with the account switchboard (W3)"
                            .to_owned(),
                    );
                }
                LauncherRow::Peers => {
                    self.flash = Some(
                        "· /peers — UI ready; lands with the mesh wave (post-v0.1)".to_owned(),
                    );
                }
            },
            // Dismissed/replaced palettes drop the click.
            Hit::PaletteRow(item) if self.palette_open() => self.activate_palette_item(item),
            Hit::MenuOption { menu, index } => {
                // Only the SAME menu the row was rendered for may answer —
                // and on the subagent screen that menu is the CHIP's card,
                // which the session projection knows nothing about (review
                // P2-7: chip-question clicks were silently dead).
                if self.screen == Screen::Subagent {
                    let card = self
                        .viewed_chip()
                        .and_then(ChipModel::question_menu)
                        .filter(|m| m.id == menu && index < m.options.len())
                        .cloned();
                    if let Some(card) = card {
                        self.menu_selection = index;
                        self.answer_chip_menu(&card);
                    }
                } else if self.screen == Screen::Session
                    && self
                        .projection
                        .open_menu()
                        .is_some_and(|m| m.id == menu && index < m.options.len())
                {
                    // The card is only answerable while its own surface is
                    // showing: Back leaves the projection (and its card)
                    // intact, so without this a queued click on the old
                    // option rect would answer an invisible card and start
                    // its parked continuation (review r2 P1-2).
                    self.menu_selection = index;
                    self.submit_menu_answer();
                }
            }
            Hit::BackChip if self.screen == Screen::Session => {
                self.back_to_launcher();
            }
            // ◉ talk (sim `speak`, tui.js:2044-2049): the mic RENDERS on the
            // launcher, but pressing it there does nothing — `speak` returns
            // unless a session is attached and idle (review r2 P2-3). The
            // screen gate is also the owning-surface guard the other hits
            // already carry (review r2 P2-4).
            Hit::TalkChip if self.screen == Screen::Session => {
                if !self.voice.enabled {
                    self.flash = Some("· enable voice first with /voice".to_owned());
                } else if !self.turn_active && !self.listening {
                    self.listening = true;
                    self.requests.push(AppRequest::Talk);
                }
            }
            Hit::HelpHint if self.screen == Screen::Launcher => self.help_open = true,
            // The SubTree panel exists only on the session/subagent screens,
            // and its rows only while it is expanded.
            Hit::ChipRow(agent)
                if matches!(self.screen, Screen::Session | Screen::Subagent)
                    && !self.subtree_collapsed =>
            {
                if let Some(path) = path_to_chip(&self.chips, &agent) {
                    self.view_path = path;
                    self.screen = Screen::Subagent;
                    self.menu_selection = 0;
                    self.scroll_back.set(0);
                }
            }
            Hit::SubTreeToggle
                if matches!(self.screen, Screen::Session | Screen::Subagent)
                    && !self.chips.is_empty() =>
            {
                self.subtree_collapsed = !self.subtree_collapsed;
            }
            Hit::TodosToggle if self.screen == Screen::Session => {
                self.todos_collapsed = !self.todos_collapsed;
            }
            // Hover-only affordance (see the variant's doc comment).
            Hit::TodoRow(_) => {}
            // The ⌂ home row and the ✕ close button belong to the subagent
            // screen; ✕ closes only the chip actually being VIEWED.
            Hit::SessionHome if self.screen == Screen::Subagent => {
                self.screen = Screen::Session;
                self.scroll_back.set(0);
            }
            Hit::ChipCloseBtn(agent)
                if self.screen == Screen::Subagent
                    && self.view_path.last() == Some(&agent)
                    && find_chip(&self.chips, &agent).is_some_and(|chip| !chip.closed) =>
            {
                self.requests.push(AppRequest::ChipClose { agent });
            }
            Hit::ChipCrumb(path) if self.screen == Screen::Subagent => {
                if path.is_empty() {
                    self.screen = Screen::Session;
                } else if path
                    .last()
                    .is_some_and(|agent| find_chip(&self.chips, agent).is_some())
                {
                    self.view_path = path;
                    self.screen = Screen::Subagent;
                }
            }
            Hit::AuraEngine if self.screen == Screen::Aura => {
                self.aura.realtime = !self.aura.realtime;
                let label = self.aura.engine_label();
                self.aura
                    .transcript
                    .push_note(format!("· engine hot-swapped → {label} · dialogue kept"));
            }
            Hit::AuraMute if self.screen == Screen::Aura => {
                self.aura.muted = !self.aura.muted;
                self.aura.transcript.push_note(
                    if self.aura.muted {
                        "· audio output muted — orchestrating silently, activity still shown"
                    } else {
                        "· audio output on"
                    }
                    .to_owned(),
                );
            }
            Hit::AuraExit if self.screen == Screen::Aura => self.exit_aura(),
            Hit::AuraTalkBtn
                if self.screen == Screen::Aura && self.aura.state == AuraState::Idle =>
            {
                self.aura.state = AuraState::Listening;
                self.requests.push(AppRequest::AuraTalk);
            }
            Hit::StickyJump(scroll_back)
                if matches!(self.screen, Screen::Session | Screen::Subagent) =>
            {
                // Stay AT the producing prompt, and suppress the sticky
                // until the next REAL wheel (sim jumpToSticky: "the bar is
                // suppressed … so it never covers the row it just
                // revealed", tui.js:2637-2657). Surface-guarded like every
                // other hit arm (Fable review D3-12).
                self.scroll_back.set(scroll_back.min(self.scroll_max.get()));
                self.sticky_suppressed = true;
            }
            // A hit whose owning surface is gone: dropped, never acted on.
            _ => {}
        }
    }

    /// Wheel scroll in the session transcript (text selection is IN-APP —
    /// drag-select + auto-copy, owner item 9; the old "left to native
    /// ⇧-drag" row is retired). Reconcile-then-apply (review r5 P2-2): the
    /// offset first
    /// folds to the last frame's truth (`scroll_max` is at most one frame
    /// stale), THEN the notch applies clamped to it — queued bursts can
    /// never bank unbounded debt, and a reversal mid-burst always moves
    /// the view. The frame's own reconcile stays as the backstop (sim
    /// reads live DOM geometry, tui.js:2648).
    pub fn handle_wheel(&mut self, up: bool) {
        if !matches!(self.screen, Screen::Session | Screen::Subagent) || self.help_open {
            return;
        }
        self.dirty = true;
        // A real scroll lifts the post-jump sticky suppression (sim
        // onTranscriptScroll → computeSticky).
        self.sticky_suppressed = false;
        let max = self.scroll_max.get();
        let current = self.scroll_back.get().min(max);
        let next = if up {
            current.saturating_add(3).min(max)
        } else {
            current.saturating_sub(3)
        };
        self.scroll_back.set(next);
    }

    /// Terminal resize: force a redraw. The frame itself reconciles the
    /// scroll offset against the new true range (review r3 P2-2 — render
    /// is the single scroll authority, so no resize-ordering bug exists).
    pub fn handle_resize(&mut self) {
        self.dirty = true;
    }

    /// Mouse motion over the frame (owner ask, TUI3a item 6). Motion
    /// events FLOOD — the model only dirties when the hovered target
    /// actually changes. Palette rows and menu options move the SELECTION
    /// on hover (sim onMouseEnter, tui.js:2992/3073); everything else is
    /// hover chrome the renderer paints from [`Self::hovered`].
    pub fn handle_hover(&mut self, hit: Option<Hit>) {
        if self.hovered == hit {
            return;
        }
        self.hovered = hit;
        self.dirty = true;
        match self.hovered.clone() {
            Some(Hit::PaletteRow(item)) => {
                if self.palette_open()
                    && let Some(position) = self.palette_items().iter().position(|i| *i == item)
                {
                    self.palette_selection = position;
                    let count = self.palette_items().len();
                    self.scroll_palette_into_view(count);
                }
            }
            Some(Hit::MenuOption { menu, index }) => {
                // Hover moves the selection on BOTH card surfaces (sim
                // `onMouseEnter` on `.imo`, tui.js:3093 — review P2-7).
                let valid = if self.screen == Screen::Subagent {
                    self.viewed_chip()
                        .and_then(ChipModel::question_menu)
                        .is_some_and(|m| m.id == menu && index < m.options.len())
                } else {
                    // Same surface gate as the click (review r2 P1-2).
                    self.screen == Screen::Session
                        && self
                            .projection
                            .open_menu()
                            .is_some_and(|m| m.id == menu && index < m.options.len())
                };
                if valid {
                    self.menu_selection = index;
                }
            }
            _ => {}
        }
    }

    fn cycle_theme(&mut self) {
        let keys = ThemeKey::ALL;
        let index = keys.iter().position(|k| *k == self.theme).unwrap_or(0);
        self.theme = keys[(index + 1) % keys.len()];
        self.flash = Some(format!("· theme → {}", self.theme.theme().label));
    }
}

/// Command-card id prefixes — each open mints `{prefix}{seq}` so a stale
/// answer can never drive a later card's consequences (review r2 P1-1).
pub const VOICE_CARD_PREFIX: &str = "voice-card-";
pub const TOOLS_CARD_PREFIX: &str = "tools-card-";

fn card_option(key: &str, label: String) -> MenuOption {
    MenuOption {
        key: key.to_owned(),
        label,
        detail: None,
        decision: None,
    }
}

/// The `/voice` menu card (sim tui.js:1824-1864, verbatim body/options).
/// Non-blocking Choice card; `origin: "voice"` selects the ◉ glyph.
#[must_use]
pub fn voice_card(voice: &VoiceState, seq: u64) -> Menu {
    let last = if voice.enabled {
        "disable voice"
    } else {
        "keep voice off"
    };
    Menu {
        id: MenuId::new(format!("{VOICE_CARD_PREFIX}{seq}")),
        kind: MenuKind::Choice,
        title: "voice — enable duplex speech for this session".to_owned(),
        body: vec![
            "input    STT provider transcribes mic → a normal user turn".to_owned(),
            "output   TTS provider speaks each assistant turn".to_owned(),
            "duplex   gpt-realtime handles both natively (barge-in, no round-trip)".to_owned(),
            "privacy  audio streams to the chosen provider only — never to the mesh".to_owned(),
        ],
        options: vec![
            card_option("whisper", "enable — Whisper STT · OpenAI TTS".to_owned()),
            card_option(
                "deepgram",
                "enable — Deepgram STT · ElevenLabs TTS".to_owned(),
            ),
            card_option(
                "realtime",
                "enable — gpt-realtime (native duplex STT+TTS)".to_owned(),
            ),
            card_option("off", last.to_owned()),
        ],
        blocking: false,
        scope: MenuScope::Session,
        origin: "voice".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

/// The `/tools` menu card (sim tui.js:1876-1906, verbatim body/options).
/// Non-blocking Choice card; `origin: "tools"` selects the ⚒ glyph.
#[must_use]
pub fn tools_card(seq: u64) -> Menu {
    Menu {
        id: MenuId::new(format!("{TOOLS_CARD_PREFIX}{seq}")),
        kind: MenuKind::Choice,
        title: "tools — core surface + custom tools".to_owned(),
        body: vec![
            "core     fs_read fs_patch process_exec agent_spawn request_input … (13, always on)"
                .to_owned(),
            "custom   notify_slack (fire-and-forget) · preview_deploy (await) · preview_smoke (deferred)"
                .to_owned(),
            "dispatch each custom tool declares a mode: how the turn treats its result".to_owned(),
            "register adding a tool is itself a menu-answerable action — a remote agent can provision another"
                .to_owned(),
        ],
        options: vec![
            card_option(
                "fire",
                "register a custom tool — fire-and-forget (dispatch, never block)".to_owned(),
            ),
            card_option(
                "await",
                "register a custom tool — await (block the turn for the result)".to_owned(),
            ),
            card_option(
                "deferred",
                "register a custom tool — deferred (returns a ticket, calls back later)".to_owned(),
            ),
            card_option("close", "close".to_owned()),
        ],
        blocking: false,
        scope: MenuScope::Session,
        origin: "tools".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}
