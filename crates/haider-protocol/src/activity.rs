//! Subagent ACTIVITY (973-activity-icons): what an agent is DOING right now,
//! as opposed to its run STATE (queued/live/waiting/done…), which already has
//! its own glyphs.
//!
//! Activity is derived ONLY from facts the daemon authored about work that
//! really ran — never from free text (task names, prose, arguments) and never
//! from a tool NAME the model merely requested:
//!
//! * live activity: an effect's typed [`EffectClass`], opened when the effect
//!   is `Dispatched` (authorized, crossing the dispatch boundary) and closed
//!   at its `Outcome`;
//! * completed activity: a `ToolCall` / `CommandExecution` item whose status
//!   is `completed` and whose activity came from a registered route or
//!   closed provider adapter, plus the daemon-authored
//!   `FileChange` / `ChildSpawn` items;
//! * `RunState` for terminal detection.
//!
//! A model that names an unadvertised, ungranted, malformed or refused tool
//! (e.g. `computer` from a child without computer use) produces no effect
//! dispatch and no successful completion, so it sets no icon and adds no
//! history.
//!
//! ## Priority
//!
//! One icon per agent: the highest-impact current activity, in this order
//! (highest first):
//!
//! | kind          | emoji | text fallback | tools                                             |
//! |---------------|-------|---------------|---------------------------------------------------|
//! | `computer`    | 🖥️    | `[screen]`    | `computer`                                        |
//! | `mobile`      | 📱    | `[phone]`     | `mobile`                                          |
//! | `web`         | 🌐    | `[web]`       | `web_fetch`, `web_search`, webextract, browser    |
//! | `remote`      | 🔌    | `[remote]`    | `ssh_shell`, remote exec                          |
//! | `edit`        | ✏️    | `[edit]`      | `fs_write`, `fs_edit`, `fs_patch`, `fs_path`      |
//! | `run`         | ⚙️    | `[run]`       | `process_exec`, `test_run`, `tool_script`, tasks  |
//! | `read`        | 🔍    | `[read]`      | `fs_read`, `fs_search`, `fs_glob`, other reads    |
//! | `orchestrate` | 🧭    | `[orch]`      | `spawn_subagent`, `message_subagent`, loom, peers |
//!
//! ## Timing (documented contract)
//!
//! * A dispatched effect is ACTIVE until its outcome (or the run's terminal
//!   state), however long it runs — a 20-minute test run shows ⚙️ throughout.
//! * A finished call stays HOT for [`ACTIVITY_HOT_WINDOW_MS`] (8 s) after its
//!   last touch. Every active or hot kind competes; the highest priority wins,
//!   so an agent that read a file and edited it within the window shows ✏️.
//! * After the hot window, with nothing in flight, the icon falls back to the
//!   MOST RECENT activity for [`ACTIVITY_LINGER_MS`] (60 s total quiet), so a
//!   model thinking between calls keeps its context; after that, no icon.
//! * An agent with live children is orchestrating (🧭 is a candidate even
//!   when quiet), because that is what a parent waiting on subagents does.
//! * A terminal agent (done/failed/cancelled) shows no current icon; the
//!   list of every activity it used ([`ActivityTracker::used`]) remains.
//!
//! The same tracker runs in the daemon (fleet snapshots, `haider agent
//! list`) and in clients over the live child stream, so both agree.

use crate::EventPayload;
use crate::effect::{AuthorizationVerdict, EffectClass, EffectPhase};
use crate::envelope::RawEnvelope;
use crate::item::{ItemEvent, ToolStatus, TurnItem};
use serde::{Deserialize, Serialize};

/// A finished call competes on priority for this long after its last touch.
pub const ACTIVITY_HOT_WINDOW_MS: u64 = 8_000;

/// With nothing active or hot, the most recent activity is still shown until
/// this much quiet has passed (measured from its last touch).
pub const ACTIVITY_LINGER_MS: u64 = 60_000;

/// Bound on concurrently tracked open calls (oldest dropped first) so a
/// journal that never closes its calls cannot grow the tracker.
pub const ACTIVITY_MAX_IN_FLIGHT: usize = 64;

/// What an agent is doing. Additive wire enum: unknown future kinds decode as
/// [`ActivityKind::Unknown`], which every renderer skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActivityKind {
    /// Desktop computer use.
    Computer,
    /// Android mobile use.
    Mobile,
    /// Web fetch/search, webextract, browser automation.
    Web,
    /// Remote shells (SSH) and remote execution.
    Remote,
    /// Mutating workspace files.
    Edit,
    /// Running commands, tests or scripts.
    Run,
    /// Read-only inspection (read/search/glob and other read tools).
    Read,
    /// Coordinating other agents (subagents, workflow graphs, looms, peers).
    Orchestrate,
    #[serde(other)]
    Unknown,
}

impl ActivityKind {
    /// Every known kind, highest priority first.
    pub const ALL: [Self; 8] = [
        Self::Computer,
        Self::Mobile,
        Self::Web,
        Self::Remote,
        Self::Edit,
        Self::Run,
        Self::Read,
        Self::Orchestrate,
    ];

    /// Priority rank: 0 is the highest impact. `Unknown` ranks last.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Computer => 0,
            Self::Mobile => 1,
            Self::Web => 2,
            Self::Remote => 3,
            Self::Edit => 4,
            Self::Run => 5,
            Self::Read => 6,
            Self::Orchestrate => 7,
            Self::Unknown => u8::MAX,
        }
    }

    /// The emoji, in emoji presentation. Glyphs whose base code point
    /// defaults to TEXT presentation (🖥 U+1F5A5, ✏ U+270F, ⚙ U+2699) carry
    /// VARIATION SELECTOR-16 so every glyph is a two-column emoji; width
    /// arithmetic must measure the whole grapheme (see [`Self::EMOJI_WIDTH`]).
    /// `Unknown` has no glyph.
    #[must_use]
    pub const fn emoji(self) -> &'static str {
        match self {
            Self::Computer => "\u{1F5A5}\u{FE0F}",
            Self::Mobile => "\u{1F4F1}",
            Self::Web => "\u{1F310}",
            Self::Remote => "\u{1F50C}",
            Self::Edit => "\u{270F}\u{FE0F}",
            Self::Run => "\u{2699}\u{FE0F}",
            Self::Read => "\u{1F50D}",
            Self::Orchestrate => "\u{1F9ED}",
            Self::Unknown => "",
        }
    }

    /// Terminal cell width of every [`Self::emoji`] glyph.
    pub const EMOJI_WIDTH: usize = 2;

    /// ASCII fallback for NO_COLOR/plain output, terminals without emoji
    /// fonts, and screen readers.
    #[must_use]
    pub const fn text_label(self) -> &'static str {
        match self {
            Self::Computer => "[screen]",
            Self::Mobile => "[phone]",
            Self::Web => "[web]",
            Self::Remote => "[remote]",
            Self::Edit => "[edit]",
            Self::Run => "[run]",
            Self::Read => "[read]",
            Self::Orchestrate => "[orch]",
            Self::Unknown => "",
        }
    }

    /// Human phrase for detail views and accessibility descriptions.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Computer => "using the computer",
            Self::Mobile => "using the phone",
            Self::Web => "on the web",
            Self::Remote => "working remotely",
            Self::Edit => "editing files",
            Self::Run => "running commands",
            Self::Read => "reading",
            Self::Orchestrate => "orchestrating",
            Self::Unknown => "",
        }
    }

    /// Stable wire name (`computer`, `mobile`, …).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Computer => "computer",
            Self::Mobile => "mobile",
            Self::Web => "web",
            Self::Remote => "remote",
            Self::Edit => "edit",
            Self::Run => "run",
            Self::Read => "read",
            Self::Orchestrate => "orchestrate",
            Self::Unknown => "unknown",
        }
    }

    const fn index(self) -> Option<usize> {
        match self {
            Self::Unknown => None,
            other => Some(other.rank() as usize),
        }
    }

    /// The activity of one typed effect class — the daemon's own authority
    /// vocabulary for what a dispatched tool is about to do.
    #[must_use]
    pub fn for_effect(class: &EffectClass) -> Option<Self> {
        Some(match class {
            EffectClass::ProviderTransport => return None,
            EffectClass::GuiAct | EffectClass::ScreenObserve | EffectClass::ScreenControl => {
                Self::Computer
            }
            EffectClass::MobileObserve | EffectClass::MobileControl | EffectClass::ReadSms => {
                Self::Mobile
            }
            // Historical recovery journals used this sentinel before the
            // dedicated ProviderTransport class existed. New recovery facts
            // use the typed class; this narrow guard makes old replay safe.
            EffectClass::Network { host } if host == "provider-request" => return None,
            EffectClass::Network { .. } => Self::Web,
            EffectClass::RemoteExecution => Self::Remote,
            EffectClass::FsWrite | EffectClass::GitOp => Self::Edit,
            EffectClass::ProcessExec => Self::Run,
            EffectClass::FsRead => Self::Read,
            EffectClass::AgentSpawn | EffectClass::PeerMessage => Self::Orchestrate,
            EffectClass::CredentialAccess => return None,
        })
    }

    /// The activity a typed turn item evidences, if any.
    #[must_use]
    pub fn for_item(item: &TurnItem) -> Option<Self> {
        match item {
            TurnItem::ToolCall { activity, .. } => *activity,
            TurnItem::CommandExecution { .. } => Some(Self::Run),
            TurnItem::FileChange { .. } => Some(Self::Edit),
            TurnItem::ChildSpawn { .. } => Some(Self::Orchestrate),
            _ => None,
        }
    }
}

/// Pick the highest-priority kind (lowest rank).
#[must_use]
pub fn highest(kinds: impl IntoIterator<Item = ActivityKind>) -> Option<ActivityKind> {
    kinds
        .into_iter()
        .filter(|kind| *kind != ActivityKind::Unknown)
        .min_by_key(|kind| kind.rank())
}

/// How clients render an activity. Resolved per client from its own setting
/// and environment; never on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivityDisplay {
    /// Two-column emoji (default).
    #[default]
    Emoji,
    /// Bracketed ASCII labels (`[read]`).
    Text,
    /// No activity glyph at all.
    Off,
}

impl ActivityDisplay {
    /// Parse a setting/env value: `emoji`/`on`, `text`/`ascii`/`plain`,
    /// `off`/`none`/`0`/`false`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "emoji" | "on" | "1" | "true" | "yes" => Some(Self::Emoji),
            "text" | "ascii" | "plain" => Some(Self::Text),
            "off" | "none" | "0" | "false" | "no" => Some(Self::Off),
            _ => None,
        }
    }

    /// Stable setting name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Emoji => "emoji",
            Self::Text => "text",
            Self::Off => "off",
        }
    }

    /// The glyph for `kind` under this display mode (`None` when off or the
    /// kind is unknown).
    #[must_use]
    pub const fn glyph(self, kind: ActivityKind) -> Option<&'static str> {
        if matches!(kind, ActivityKind::Unknown) {
            return None;
        }
        match self {
            Self::Emoji => Some(kind.emoji()),
            Self::Text => Some(kind.text_label()),
            Self::Off => None,
        }
    }

    /// Resolve the display mode from an explicit override value
    /// (`HAIDER_ACTIVITY_ICONS`), the persisted setting, `NO_COLOR`, and
    /// `TERM`. Precedence: override > setting > environment heuristics.
    /// `NO_COLOR` (any value) and `TERM` of `dumb`/`linux`/`vt*` — consoles
    /// that commonly lack emoji fonts — fall back to text labels.
    #[must_use]
    pub fn resolve(
        override_value: Option<&str>,
        setting: Option<Self>,
        no_color: bool,
        term: Option<&str>,
    ) -> Self {
        if let Some(explicit) = override_value.and_then(Self::parse) {
            return explicit;
        }
        if let Some(setting) = setting {
            return setting;
        }
        let limited_term = term.is_some_and(|term| {
            term == "dumb" || term == "linux" || term.starts_with("vt") || term.is_empty()
        });
        if no_color || limited_term {
            Self::Text
        } else {
            Self::Emoji
        }
    }
}

/// One kind's last-touch instant (wire form of the tracker state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityTouchV1 {
    pub kind: ActivityKind,
    pub at_ms: u64,
}

/// The reducer's time-independent state, published on the wire (inside
/// `AgentMetricsSnapshot::activity`) so a client evaluates
/// [`ActivityTracker::current`] against ITS clock — the icon keeps decaying
/// on a quiet screen without another daemon frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityStateV1 {
    /// Kinds with a call that started and has not completed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<ActivityKind>,
    /// Last touch per used kind, highest priority first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched: Vec<ActivityTouchV1>,
    /// The most recent activity (the quiet fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<ActivityTouchV1>,
    /// The latest run reached a terminal state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminal: bool,
}

/// Live activity reducer over one agent's own journal facts. Feed it every
/// envelope/payload attributed to the agent, in journal order, with the
/// envelope's `committed_at_ms`; ask [`Self::current`] at any instant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityTracker {
    /// Dispatched, unfinished effects: (effect id, kind). Bounded by
    /// [`ACTIVITY_MAX_IN_FLIGHT`].
    in_flight: Vec<(String, ActivityKind)>,
    /// Delegated children spawned and not yet reported (agent ids).
    children: Vec<String>,
    /// Journaled but not yet dispatched effect intents: (effect id, kind).
    /// Never shown; promoted to `in_flight` only on `Dispatched`.
    pending: Vec<(String, ActivityKind)>,
    /// Per kind (indexed by rank): the last instant it was touched.
    last_touch: [Option<u64>; 8],
    /// The most recently STARTED/observed kind and when.
    latest: Option<(ActivityKind, u64)>,
    /// The run reached a terminal state (cleared by a later non-terminal run).
    terminal: bool,
}

impl ActivityTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn touch(&mut self, kind: ActivityKind, at_ms: u64) {
        let Some(index) = kind.index() else {
            return;
        };
        let slot = &mut self.last_touch[index];
        *slot = Some(slot.map_or(at_ms, |held| held.max(at_ms)));
        if self.latest.is_none_or(|(_, held)| at_ms >= held) {
            self.latest = Some((kind, at_ms));
        }
    }

    /// Apply one item lifecycle event. A `ToolCall` item carries the name
    /// the MODEL requested — it may be unadvertised, ungranted, malformed or
    /// refused — so it never opens or touches an activity on its own. Only a
    /// SUCCESSFULLY completed call (which proves a real route ran; this also
    /// covers provider-executed server tools, whose only fact is their
    /// completed item) and the daemon-authored typed items count. Live
    /// in-flight activity comes from dispatched effects (see
    /// [`Self::observe_effect`]).
    pub fn observe_item(&mut self, event: &ItemEvent, at_ms: u64) {
        let ItemEvent::Completed { item, .. } = event else {
            return;
        };
        // A delegated child is outstanding from the daemon-authored
        // `ChildSpawn` fact until its `ChildResult`: the parent is
        // orchestrating the whole time it waits.
        match item {
            TurnItem::ChildSpawn { agent }
                if !self.children.iter().any(|open| open == agent.as_str()) =>
            {
                if self.children.len() >= ACTIVITY_MAX_IN_FLIGHT {
                    self.children.remove(0);
                }
                self.children.push(agent.as_str().to_owned());
            }
            TurnItem::ChildResult { report } => {
                self.children.retain(|open| open != report.agent.as_str());
                self.touch(ActivityKind::Orchestrate, at_ms);
                return;
            }
            _ => {}
        }
        let counts = match item {
            TurnItem::ToolCall { status, .. } | TurnItem::CommandExecution { status, .. } => {
                *status == ToolStatus::Completed
            }
            TurnItem::FileChange { .. } | TurnItem::ChildSpawn { .. } => true,
            _ => false,
        };
        if counts && let Some(kind) = ActivityKind::for_item(item) {
            self.terminal = false;
            self.touch(kind, at_ms);
        }
    }

    /// Apply one effect lifecycle fact. The daemon journals an effect's
    /// typed class before authorization; the activity OPENS only when the
    /// effect is `Dispatched` (authorized and actually crossing the dispatch
    /// boundary) and closes at its `Outcome`. Refused, denied, never-asked or
    /// never-dispatched effects set no icon and add no history.
    pub fn observe_effect(&mut self, phase: &EffectPhase, at_ms: u64) {
        match phase {
            EffectPhase::Intent(intent) => {
                if let Some(kind) = ActivityKind::for_effect(&intent.class) {
                    let id = intent.effect.as_str().to_owned();
                    self.pending.retain(|(held, _)| *held != id);
                    if self.pending.len() >= ACTIVITY_MAX_IN_FLIGHT {
                        self.pending.remove(0);
                    }
                    self.pending.push((id, kind));
                }
            }
            EffectPhase::Authorized { effect, verdict } => {
                if matches!(verdict, AuthorizationVerdict::Deny { .. }) {
                    self.pending.retain(|(held, _)| held != effect.as_str());
                }
            }
            EffectPhase::Dispatched { effect } => {
                let Some(position) = self
                    .pending
                    .iter()
                    .position(|(held, _)| held == effect.as_str())
                else {
                    return;
                };
                let (id, kind) = self.pending.remove(position);
                self.terminal = false;
                self.touch(kind, at_ms);
                if !self.in_flight.iter().any(|(open, _)| *open == id) {
                    if self.in_flight.len() >= ACTIVITY_MAX_IN_FLIGHT {
                        self.in_flight.remove(0);
                    }
                    self.in_flight.push((id, kind));
                }
            }
            EffectPhase::Outcome { effect, .. } => {
                self.pending.retain(|(held, _)| held != effect.as_str());
                if let Some(position) = self
                    .in_flight
                    .iter()
                    .position(|(open, _)| open == effect.as_str())
                {
                    let (_, kind) = self.in_flight.remove(position);
                    self.touch(kind, at_ms);
                }
            }
        }
    }

    /// Apply one decoded payload.
    pub fn observe(&mut self, payload: &EventPayload, at_ms: u64) {
        match payload {
            EventPayload::Item(event) => self.observe_item(event, at_ms),
            EventPayload::Effect(phase) => self.observe_effect(phase, at_ms),
            EventPayload::RunState(state) => {
                if state.is_terminal() {
                    self.terminal = true;
                    self.in_flight.clear();
                    self.pending.clear();
                    self.children.clear();
                } else {
                    self.terminal = false;
                }
            }
            _ => {}
        }
    }

    /// Apply one raw envelope, decoding only the payload kinds that can carry
    /// activity (items and run states) — cheap enough for full-journal folds.
    pub fn observe_raw(&mut self, envelope: &RawEnvelope) {
        let kind = envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str);
        if !matches!(kind, Some("item" | "run_state" | "effect")) {
            return;
        }
        if kind == Some("item") {
            let item_kind = envelope
                .payload
                .get("item")
                .and_then(|item| item.get("item"))
                .and_then(serde_json::Value::as_str);
            if !matches!(
                item_kind,
                Some(
                    "tool_call"
                        | "command_execution"
                        | "file_change"
                        | "child_spawn"
                        | "child_result"
                )
            ) {
                return;
            }
        }
        if let Ok(payload) = envelope.payload.decode_event() {
            self.observe(&payload, envelope.committed_at_ms);
        }
    }

    /// Mark the agent terminal from an external state source (e.g. a chip's
    /// display state) — clears open calls.
    pub fn set_terminal(&mut self, terminal: bool) {
        self.terminal = terminal;
        if terminal {
            self.in_flight.clear();
        }
    }

    /// Whether any activity was ever observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.latest.is_none()
    }

    /// The current activity at `now_ms` (see the module timing contract).
    /// `live_children`: the agent has non-terminal subagents of its own.
    #[must_use]
    pub fn current(&self, now_ms: u64, live_children: bool) -> Option<ActivityKind> {
        if self.terminal {
            return None;
        }
        let hot = ActivityKind::ALL.into_iter().filter(|kind| {
            kind.index()
                .and_then(|index| self.last_touch[index])
                .is_some_and(|at| now_ms.saturating_sub(at) <= ACTIVITY_HOT_WINDOW_MS)
        });
        let open = self.in_flight.iter().map(|(_, kind)| *kind);
        let orchestrating =
            (live_children || !self.children.is_empty()).then_some(ActivityKind::Orchestrate);
        if let Some(best) = highest(hot.chain(open).chain(orchestrating)) {
            return Some(best);
        }
        self.latest
            .filter(|(_, at)| now_ms.saturating_sub(*at) <= ACTIVITY_LINGER_MS)
            .map(|(kind, _)| kind)
    }

    /// The time-independent wire state.
    #[must_use]
    pub fn state(&self) -> ActivityStateV1 {
        let mut open: Vec<ActivityKind> = self.in_flight.iter().map(|(_, kind)| *kind).collect();
        if !self.children.is_empty() {
            open.push(ActivityKind::Orchestrate);
        }
        open.sort_by_key(|kind| kind.rank());
        open.dedup();
        ActivityStateV1 {
            open,
            touched: ActivityKind::ALL
                .into_iter()
                .filter_map(|kind| {
                    let at_ms = self.last_touch[kind.index()?]?;
                    Some(ActivityTouchV1 { kind, at_ms })
                })
                .collect(),
            latest: self
                .latest
                .map(|(kind, at_ms)| ActivityTouchV1 { kind, at_ms }),
            terminal: self.terminal,
        }
    }

    /// Rebuild a tracker from its wire state (open calls keep synthetic ids).
    #[must_use]
    pub fn from_state(state: &ActivityStateV1) -> Self {
        let mut tracker = Self::new();
        for (index, kind) in state.open.iter().enumerate() {
            if tracker.in_flight.len() < ACTIVITY_MAX_IN_FLIGHT && *kind != ActivityKind::Unknown {
                tracker.in_flight.push((format!("wire-{index}"), *kind));
            }
        }
        for touch in &state.touched {
            if let Some(index) = touch.kind.index() {
                tracker.last_touch[index] = Some(touch.at_ms);
            }
        }
        tracker.latest = state
            .latest
            .filter(|touch| touch.kind != ActivityKind::Unknown)
            .map(|touch| (touch.kind, touch.at_ms));
        tracker.terminal = state.terminal;
        tracker
    }

    /// Every activity observed so far, highest priority first.
    #[must_use]
    pub fn used(&self) -> Vec<ActivityKind> {
        ActivityKind::ALL
            .into_iter()
            .filter(|kind| {
                kind.index()
                    .is_some_and(|index| self.last_touch[index].is_some())
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
