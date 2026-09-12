//! Collapsed tool rows — the transcript's readability law (971-tui-collapse).
//!
//! Reference: Claude Code's TUI renders a tool call as ONE summary row plus
//! an indented `└` sub-line, folds a run of consecutive same-tool calls into
//! `Ran N shell commands`, and keeps the full output one keystroke away.
//! Owner (2026-09-08): "tool responses should be compressed by default …
//! then expand if needed by the user (can go back to compressed from the
//! main TUI)".
//!
//! This module is the PURE half: state, the row-format builder, the
//! fold-run scan, and the colour-BY-MEANING tokenizer. It owns no ratatui
//! and no theme colours — segments carry a [`Tone`], and `style.rs` is the
//! single seam where a tone becomes ink (`every_surface_uses_theme_slots`
//! keeps it that way). `render.rs` inks and lays out; `app.rs` owns the
//! state and the gestures; `settings.rs` persists the mode.
//!
//! Sibling idioms: `AppModel::todos_collapsed` (render.rs:6336) and
//! `subtree_collapsed` (render.rs:8041) — a collapsed header that summarises
//! what it hides, a `▸`/`▾` gesture, and per-session slot persistence
//! (`session::SessionState`).

use std::collections::{BTreeMap, BTreeSet};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// How much of a tool call the transcript shows BEFORE anyone touches it.
/// Persisted per session in `tui-settings.json` so an orchestration run stays
/// quiet and a debugging run stays verbose across restarts.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Verbosity {
    /// Summary row only — no `└` sub-line, no output. The orchestration mode.
    Quiet,
    /// Summary row + the first meaningful result line. The owner's default.
    #[default]
    Normal,
    /// Every row expanded on arrival, runs never folded. The debugging mode.
    Verbose,
}

impl Verbosity {
    /// The persisted/`/verbosity` names, in cycle order.
    pub const ALL: [Verbosity; 3] = [Verbosity::Quiet, Verbosity::Normal, Verbosity::Verbose];

    /// Parse a persisted or typed name. Unknown names fall back to the
    /// default at the call site — never a half-applied mode.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "quiet" => Some(Self::Quiet),
            "default" | "normal" => Some(Self::Normal),
            "verbose" => Some(Self::Verbose),
            _ => None,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Normal => "default",
            Self::Verbose => "verbose",
        }
    }

    /// The next mode in the ⌥V cycle: quiet → default → verbose → quiet.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Quiet => Self::Normal,
            Self::Normal => Self::Verbose,
            Self::Verbose => Self::Quiet,
        }
    }

    /// Verbose opens every row on arrival; the other two collapse by law.
    #[must_use]
    pub const fn opens_rows(self) -> bool {
        matches!(self, Self::Verbose)
    }

    /// Quiet drops the `└` sub-line — the summary row stands alone.
    #[must_use]
    pub const fn shows_subline(self) -> bool {
        !matches!(self, Self::Quiet)
    }
}

/// One tool row's disclosure state. ⏎/Space/click CYCLES it, so a reader
/// who over-expands gets back to the compressed view with the same gesture
/// that opened it (the owner's "can go back to compressed" rule).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RowState {
    /// Summary row (+ `└` sub-line outside quiet).
    #[default]
    Collapsed,
    /// Summary row + a BOUNDED output region ([`EXPANDED_MAX_ROWS`]) that
    /// ends in a `show all` affordance when it hid anything.
    Expanded,
    /// Summary row + every retained output row.
    ShowAll,
}

impl RowState {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Collapsed => Self::Expanded,
            Self::Expanded => Self::ShowAll,
            Self::ShowAll => Self::Collapsed,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Collapsed => "collapsed",
            Self::Expanded => "expanded",
            Self::ShowAll => "show_all",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "collapsed" => Some(Self::Collapsed),
            "expanded" => Some(Self::Expanded),
            "show_all" => Some(Self::ShowAll),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_collapsed(self) -> bool {
        matches!(self, Self::Collapsed)
    }
}

/// What the reader asked of EVERY tool row at once, which is a different
/// question from what the verbosity mode defaults to (verify 1, F1).
///
/// The round-2 boolean could not express "collapse everything" while the
/// mode was `verbose`: the mode's default won and the blanket lost, so
/// ⌃O and `/collapse all` looked inert in verbose. A blanket the reader
/// states EXPLICITLY now outranks the mode; `Mode` is the resting state
/// that defers to it, and changing the mode returns to `Mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Blanket {
    /// No blanket asked for — the verbosity mode's default stands.
    #[default]
    Mode,
    /// Every untouched row collapsed, whatever the mode says.
    Collapsed,
    /// Every untouched row expanded, whatever the mode says.
    Expanded,
}

impl Blanket {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Mode => "mode",
            Self::Collapsed => "collapsed",
            Self::Expanded => "expanded",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "mode" => Some(Self::Mode),
            "collapsed" => Some(Self::Collapsed),
            "expanded" => Some(Self::Expanded),
            _ => None,
        }
    }
}

/// DISPLAY rows an [`RowState::Expanded`] tool row shows before it yields
/// to the `show all` affordance. Bounded so one chatty tool can never take
/// the viewport hostage — and scrollable WITHIN that bound (verify 1, F2),
/// so every retained row is reachable without opening the whole tail.
pub const EXPANDED_MAX_ROWS: usize = 10;

/// A run must be at least this long to fold into `Ran N …`.
pub const FOLD_MIN: usize = 2;

/// Sessions the profile store keeps disclosure state for (verify 1, F3).
/// Bounded so a long-lived profile can never grow `tui-settings.json`
/// without limit; the oldest key yields when a ninth session commits.
pub const MAX_PERSISTED_SESSIONS: usize = 8;

/// Per-row disclosure states one session's slot retains.
///
/// Scope, stated exactly: these live in `session::SessionState`, so they
/// survive leaving a session and coming back to it within a run (the A→B→A
/// checkout law), AND — since verify 1 (F3) — in `tui-settings.json` keyed
/// by session id, so they survive a process restart. The slot stays
/// authoritative once a session has been opened in this process; the store
/// is what the first open after a restart falls back to.
pub const MAX_PERSISTED_ROWS: usize = 128;

/// The spinner a streaming row wears, driven by the SHARED animation clock
/// (`AppModel::anim_phase`) — no new timer, the zero-idle-wakeup law. The
/// frames stay inside the `◐` family the completed glyphs already use
/// (`plain::status_glyph`), so a live row reads as the same vocabulary mid-turn.
pub const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// The spinner frame for `phase`.
#[must_use]
pub fn spinner_frame(phase: u8) -> &'static str {
    SPINNER[(phase as usize) % SPINNER.len()]
}

/// Spinner glyph selected for the current motion preference.
///
/// Screen readers and users who prefer reduced motion still get the same
/// live-row state, but the glyph stays stable so successive frames do not
/// create needless terminal churn. The rich renderer passes `false`; plain
/// and accessibility consumers can opt into the stable form without adding a
/// second animation clock.
#[must_use]
pub fn spinner_frame_for(phase: u8, reduced_motion: bool) -> &'static str {
    if reduced_motion {
        SPINNER[0]
    } else {
        spinner_frame(phase)
    }
}

/// A tool call's duration. [`crate::format::fmt_elapsed`] is built for
/// minutes and hours and floors everything under a second to `0s` — which on
/// a tool row reads exactly like the fabricated zero this wave refuses to
/// print. Sub-second calls (a `fs_read`, a `grep`) therefore report their
/// milliseconds; from a second up, the shared elapsed vocabulary takes over.
#[must_use]
pub fn fmt_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        crate::format::fmt_elapsed(ms)
    }
}

/// What a segment MEANS. `style.rs` maps each to a theme slot; nothing here
/// knows a colour. Colouring by meaning rather than by source is owner item
/// (3): a non-zero exit code, a SHIP/HOLD/NO-SHIP verdict, a `file:line` and
/// a thread/run id each keep their own ink wherever they appear.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tone {
    /// Body ink — agent prose and expanded output at full readability.
    Body,
    /// Secondary ink (`dim`): metadata that must STAY readable — the 4.5:1
    /// floor `ui_themes_tests` now pins on every theme's ground. The
    /// DEFAULT tone: anything the tokenizer cannot classify is metadata,
    /// never decoration.
    #[default]
    Meta,
    /// Barely-there ink: the `└` elbow and the row's structural glyphs only.
    Structure,
    /// The identity ink: tool names, thread/run/agent ids.
    Name,
    /// Emphasis: the fold row's count, the focused affordance.
    Emphasis,
    /// The accent: `file:line` coordinates and the `show all` door.
    Accent,
    Ok,
    Warn,
    Err,
}

/// One inked run of text on a tool row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Segment {
    pub text: String,
    pub tone: Tone,
}

impl Segment {
    #[must_use]
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }

    #[must_use]
    pub fn width(&self) -> usize {
        self.text.chars().count()
    }
}

/// Total display width of a segment row.
#[must_use]
pub fn segments_width(segments: &[Segment]) -> usize {
    segments.iter().map(Segment::width).sum()
}

/// The plain text of a segment row — the greppable twin the pins assert on.
#[must_use]
pub fn segments_text(segments: &[Segment]) -> String {
    segments.iter().map(|s| s.text.as_str()).collect()
}

// ---------------------------------------------------------------- state ---

/// Every reader-owned disclosure decision for one transcript.
///
/// `revision` is the cache coordinate: `TranscriptLayoutCache::reconcile`
/// treats a bump exactly like a width or theme change, because collapsing a
/// row changes its measured height and therefore every row start below it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolFold {
    verbosity: Verbosity,
    /// ⌥T / ⌃O / `/collapse`'s blanket, which OUTRANKS the verbosity
    /// default when the reader stated one (verify 1, F1).
    blanket: Blanket,
    /// Rows whose reader-set state overrides the default. Keyed by the item
    /// id STRING (`ItemId` is `Hash`, not `Ord`; a `BTreeMap` keeps the
    /// persisted file byte-stable).
    rows: BTreeMap<String, RowState>,
    /// Fold heads the reader opened — the run shows its member rows again.
    unfolded: BTreeSet<String>,
    /// First DISPLAY row the bounded expanded window shows, per row — the
    /// internal scroll verify 1 (F2) found missing. Absent means the top.
    scroll: BTreeMap<String, usize>,
    /// The keyboard-focused tool row, if any. `None` is the resting state:
    /// the composer keeps ⏎/Space until the reader asks for a row with ⌥N/⌥P
    /// or points at one, so this can never steal a submit.
    focus: Option<String>,
    revision: u64,
}

impl ToolFold {
    #[must_use]
    pub const fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    #[must_use]
    pub const fn blanket(&self) -> Blanket {
        self.blanket
    }

    /// True when every untouched row is currently open — the blanket the
    /// reader asked for, or the mode's default when they asked for none.
    #[must_use]
    pub const fn all_expanded(&self) -> bool {
        match self.blanket {
            Blanket::Expanded => true,
            Blanket::Collapsed => false,
            Blanket::Mode => self.verbosity.opens_rows(),
        }
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn focus(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    /// The state this row renders in: the reader's override if it set one,
    /// else the default the mode dictates. DEFAULT COLLAPSED is the law —
    /// only `Verbose` or a ⌥T override opens a row nobody touched.
    #[must_use]
    pub fn state_of(&self, item_id: &str) -> RowState {
        if let Some(state) = self.rows.get(item_id) {
            return *state;
        }
        if self.all_expanded() {
            RowState::Expanded
        } else {
            RowState::Collapsed
        }
    }

    /// True while this run head's fold is open (its members render on their
    /// own rows).
    #[must_use]
    pub fn is_unfolded(&self, item_id: &str) -> bool {
        self.unfolded.contains(item_id)
    }

    /// ⏎/Space/click on one row: collapsed → expanded → show all → collapsed.
    pub fn cycle(&mut self, item_id: &str) {
        let next = self.state_of(item_id).next();
        self.set(item_id, next);
    }

    /// Put one row in an exact state (the `show all` door, and restore).
    pub fn set(&mut self, item_id: &str, state: RowState) {
        self.rows.insert(item_id.to_owned(), state);
        // A row that closed, or opened whole, starts its window at the top:
        // a stale offset would open it part-way down for no reason.
        if !matches!(state, RowState::Expanded) {
            self.scroll.remove(item_id);
        }
        self.prune_rows();
        self.bump();
    }

    /// Open a folded run so its members render individually. Folding again
    /// is the same gesture.
    pub fn toggle_fold(&mut self, head_id: &str) {
        if !self.unfolded.remove(head_id) {
            self.unfolded.insert(head_id.to_owned());
        }
        self.bump();
    }

    /// ⌥T / ⌃O — every tool row in the transcript at once, from whatever
    /// they collectively show now.
    pub fn toggle_all(&mut self) {
        self.set_blanket(if self.all_expanded() {
            Blanket::Collapsed
        } else {
            Blanket::Expanded
        });
    }

    /// `/collapse all|expand` — the blanket stated ABSOLUTELY, so a typed
    /// command is idempotent.
    ///
    /// Verify 1 (F1) found two holes this closes: the round-2 version
    /// no-opped when the boolean already matched, leaving contrary per-row
    /// overrides open behind a "collapsed" flash; and it could not beat the
    /// `verbose` default at all. The overrides are DROPPED unconditionally —
    /// the gesture is genuinely all-or-nothing, or the reader is pressing it
    /// at a row that refuses — and an explicit blanket outranks the mode.
    pub fn set_blanket(&mut self, blanket: Blanket) {
        self.blanket = blanket;
        self.rows.clear();
        self.unfolded.clear();
        self.scroll.clear();
        self.bump();
    }

    /// ⌥V — the persisted quiet/default/verbose cycle. Changing the mode
    /// also drops per-row overrides: the mode IS the new default, and a
    /// stale override would hide it.
    pub fn set_verbosity(&mut self, verbosity: Verbosity) {
        if self.verbosity == verbosity {
            return;
        }
        self.verbosity = verbosity;
        // The MODE is the new default, so the blanket returns to deferring
        // to it and stale overrides go with it.
        self.blanket = Blanket::Mode;
        self.rows.clear();
        self.unfolded.clear();
        self.scroll.clear();
        self.bump();
    }

    /// Seed the mode from the persisted profile at boot WITHOUT counting as
    /// a reader commit (the settings store writes on a commit counter).
    pub fn seed_verbosity(&mut self, verbosity: Verbosity) {
        self.verbosity = verbosity;
        self.bump();
    }

    /// Move the keyboard focus (⌥N/⌥P, hover, click). `None` returns the
    /// composer its ⏎/Space.
    pub fn set_focus(&mut self, item_id: Option<&str>) {
        let next = item_id.map(str::to_owned);
        if self.focus != next {
            self.focus = next;
            self.bump();
        }
    }

    /// Restore a session's disclosure state — from its in-process slot (the
    /// A→B→A checkout law) or from the profile store after a restart
    /// (verify 1, F3).
    pub fn restore(&mut self, blanket: Blanket, rows: BTreeMap<String, RowState>) {
        self.blanket = blanket;
        self.rows = rows;
        self.rows_truncate();
        self.unfolded.clear();
        self.scroll.clear();
        self.focus = None;
        self.bump();
    }

    /// The persistable per-row overrides (the mode is saved alongside them).
    #[must_use]
    pub fn rows_snapshot(&self) -> BTreeMap<String, RowState> {
        self.rows.clone()
    }

    /// True when this session has disclosure state worth persisting.
    #[must_use]
    pub fn has_session_state(&self) -> bool {
        self.blanket != Blanket::Mode || !self.rows.is_empty()
    }

    // ---- F2: the bounded window's internal scroll ----

    /// First DISPLAY row the bounded expanded window shows for this row.
    #[must_use]
    pub fn scroll_of(&self, item_id: &str) -> usize {
        self.scroll.get(item_id).copied().unwrap_or(0)
    }

    /// Scroll one row's bounded window, clamped to `max` (the caller knows
    /// how many display rows the retained output actually wraps to, so the
    /// clamp cannot drift from what is on screen).
    pub fn scroll_row(&mut self, item_id: &str, delta: isize, max: usize) {
        let current = self.scroll_of(item_id);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs())
        }
        .min(max);
        if next == current {
            return;
        }
        if next == 0 {
            self.scroll.remove(item_id);
        } else {
            self.scroll.insert(item_id.to_owned(), next);
        }
        self.bump();
    }

    /// Clear transcript-local row choices while retaining the current mode.
    /// Session checkout restores the destination's mode separately.
    pub fn clear_session(&mut self) {
        self.blanket = Blanket::Mode;
        self.rows.clear();
        self.unfolded.clear();
        self.scroll.clear();
        self.focus = None;
        self.bump();
    }

    fn prune_rows(&mut self) {
        if self.rows.len() > MAX_PERSISTED_ROWS {
            self.rows_truncate();
        }
    }

    fn rows_truncate(&mut self) {
        while self.rows.len() > MAX_PERSISTED_ROWS {
            let Some(first) = self.rows.keys().next().cloned() else {
                break;
            };
            self.rows.remove(&first);
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// When a tool call started and (once it lands) finished, on the shared
/// `AppModel::clock_ms` wall clock — the same clock `ChipModel::elapsed_ms`
/// rides. The protocol carries NO tool duration, so this is a client-side
/// observation: a row whose start was never observed (a replayed history, a
/// restart mid-run) simply drops the duration segment rather than printing a
/// fabricated `0s`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolTiming {
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
}

impl ToolTiming {
    #[must_use]
    pub const fn started(started_ms: u64) -> Self {
        Self {
            started_ms,
            ended_ms: None,
        }
    }

    /// Elapsed against `now_ms`, frozen once the call landed. Saturating
    /// both ways: clock skew renders `0s`, never a wrapped figure.
    #[must_use]
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        self.ended_ms
            .unwrap_or(now_ms)
            .max(self.started_ms)
            .saturating_sub(self.started_ms)
    }
}

// ------------------------------------------------------------ row format ---

/// Everything the row format needs about one tool call, lifted out of the
/// projection so the builder stays pure and testable without a frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowFacts<'a> {
    /// The tool's name (`bash`, `fs_edit`, `web_fetch`, …), or the command
    /// itself on a `CommandExecution` row.
    pub name: String,
    /// The tone the name wears: the identity ink for a tool, body ink for a
    /// literal shell command (which is text the reader reads, not a label).
    pub name_tone: Tone,
    /// The argument summary shown inside the parentheses. Empty means no
    /// parentheses at all, never `()`.
    pub args: String,
    /// The status glyph, already resolved by the caller.
    pub glyph: &'static str,
    /// The tone the glyph wears.
    pub glyph_tone: Tone,
    /// `Some` only where the MODEL actually carries one (`CommandExecution`);
    /// the protocol has no exit code on a generic tool call, and this row
    /// invents none.
    pub exit_code: Option<i32>,
    /// A non-`Completed` terminal status as a toned word — `failed` in the
    /// error ink, `cancelled` in metadata ink (a cancellation is an
    /// outcome, never a failure: the frozen `ToolStatus` law).
    pub outcome: Option<Segment>,
    /// Retained output lines. Zero means the segment is ABSENT, never `0 lines`.
    pub output_lines: usize,
    /// Elapsed milliseconds, when the client actually observed the start.
    pub elapsed_ms: Option<u64>,
    /// True while the call is open.
    pub streaming: bool,
    /// May the SPINNER replace the glyph while the call is open? True for a
    /// tool call, whose glyph is a status marker. FALSE for a shell command,
    /// whose `$`/`!` sigil is PROVENANCE — a user `!` command must not stop
    /// looking like one just because it is still running; `· running…`
    /// carries the liveness there.
    pub spinner: bool,
    /// The bounded terminal reason, when the projection joined one.
    pub reason: Option<&'a str>,
}

/// The collapsed summary row: `⟨glyph⟩ name(args) · exit N · N lines · Ns`.
///
/// The argument summary is the only ELASTIC segment: every fixed segment is
/// measured first and the remainder is the summary's budget, so a narrow
/// terminal loses the arguments before it loses the exit code — the figure a
/// reader is actually scanning for. `width == 0` disables budgeting, which is
/// what the format pins assert against.
#[must_use]
pub fn summary_segments(facts: &RowFacts<'_>, phase: u8, width: usize) -> Vec<Segment> {
    summary_segments_with_motion(facts, phase, width, false)
}

/// Build a tool summary while honoring a caller's reduced-motion preference.
/// Rich rendering keeps the historical animated default; plain and
/// accessibility surfaces can request a stable running glyph while retaining
/// the same status and timing facts.
#[must_use]
pub fn summary_segments_with_motion(
    facts: &RowFacts<'_>,
    phase: u8,
    width: usize,
    reduced_motion: bool,
) -> Vec<Segment> {
    let mut head = vec![
        Segment::new("  ", Tone::Structure),
        Segment::new(
            format!(
                "{} ",
                if facts.streaming && facts.spinner {
                    spinner_frame_for(phase, reduced_motion)
                } else {
                    facts.glyph
                }
            ),
            facts.glyph_tone,
        ),
        Segment::new(facts.name.clone(), facts.name_tone),
    ];
    let mut tail: Vec<Segment> = Vec::new();
    let mut failing = false;
    if facts.streaming {
        tail.push(Segment::new(" · running…", Tone::Meta));
        if let Some(ms) = facts.elapsed_ms {
            tail.push(Segment::new(format!(" {}", fmt_duration(ms)), Tone::Meta));
        }
    } else {
        if let Some(code) = facts.exit_code {
            failing = code != 0;
            tail.push(Segment::new(
                format!(" · exit {code}"),
                if failing { Tone::Err } else { Tone::Meta },
            ));
        } else if let Some(outcome) = facts.outcome.clone() {
            failing = outcome.tone == Tone::Err;
            tail.push(Segment::new(" · ", outcome.tone));
            tail.push(outcome);
        }
        if facts.output_lines > 0 {
            tail.push(Segment::new(
                format!(
                    " · {} line{}",
                    facts.output_lines,
                    if facts.output_lines == 1 { "" } else { "s" }
                ),
                Tone::Meta,
            ));
        }
        if let Some(ms) = facts.elapsed_ms {
            tail.push(Segment::new(format!(" · {}", fmt_duration(ms)), Tone::Meta));
        }
    }
    if let Some(reason) = facts.reason.filter(|reason| !reason.is_empty()) {
        // E8 visual pass: a reason on a SETTLED, non-failing row is a
        // recovered in-flight retry ("transient web_fetch failure — retry
        // 2/2 succeeded") — quiet metadata, never an alarming tone. Only a
        // failing row's reason wears the error ink.
        tail.push(Segment::new(
            format!(" · {reason}"),
            if failing { Tone::Err } else { Tone::Meta },
        ));
    }
    if !facts.args.is_empty() {
        // `(` and `)` cost two cells; below that the parentheses are
        // dropped WHOLE rather than rendered around an empty summary.
        let budget = if width == 0 {
            facts.args.chars().count()
        } else {
            width
                .saturating_sub(segments_width(&head))
                .saturating_sub(segments_width(&tail))
                .saturating_sub(2)
        };
        if budget > 0 {
            head.push(Segment::new(
                format!("({})", ellipsize(&facts.args, budget)),
                Tone::Meta,
            ));
        }
    }
    head.extend(tail);
    head
}

/// The `└` sub-line under a collapsed row: the result's first MEANINGFUL
/// line, coloured by meaning. `None` when the call produced nothing worth a
/// line — the summary row then stands alone rather than growing an empty elbow.
#[must_use]
pub fn subline_segments(output: &str, width: usize) -> Option<Vec<Segment>> {
    subline_segments_from(output, false, width)
}

/// As [`subline_segments`], but told whether the retained tail was CUT at
/// the front. It was, when the 8 KiB output cap dropped earlier output — and
/// then the tail's first line is a mid-line FRAGMENT (`ine 0051 — …`), which
/// is worse than no elbow at all. Skip it and speak with the first whole
/// line instead.
#[must_use]
pub fn subline_segments_from(
    output: &str,
    tail_was_cut: bool,
    width: usize,
) -> Option<Vec<Segment>> {
    let body = if tail_was_cut {
        let (_, rest) = output.split_once('\n')?;
        rest
    } else {
        output
    };
    let line = first_meaningful_line(body)?;
    let mut segments = vec![Segment::new("    └ ", Tone::Structure)];
    let budget = if width == 0 {
        line.chars().count()
    } else {
        width.saturating_sub(6)
    };
    if budget == 0 {
        return None;
    }
    segments.extend(meaning_segments(&ellipsize(line, budget)));
    Some(segments)
}

/// The folded run's row: `Ran 3 shell commands`. The count is the only
/// EMPHASIS on the row — it is the number the reader is deciding about.
///
/// A run whose members had output DROPPED or UNDECODABLE says so here. The
/// honesty markers are not disclosure-gated anywhere else in this wave, and
/// a fold row that swallowed them would be quietly claiming a completeness
/// the client never had.
#[must_use]
pub fn fold_segments(run: &FoldRun) -> Vec<Segment> {
    let mut segments = vec![
        Segment::new("  ", Tone::Structure),
        Segment::new("Ran ", Tone::Meta),
        Segment::new(run.len.to_string(), Tone::Emphasis),
        Segment::new(format!(" {}", fold_noun(&run.name, run.len)), Tone::Meta),
    ];
    if run.truncated {
        segments.push(Segment::new(
            if run.len == 1 {
                " · bounded tail"
            } else {
                " · bounded tails"
            },
            Tone::Meta,
        ));
    }
    if run.decode_error {
        segments.push(Segment::new(" · ⚠ undecodable output", Tone::Warn));
    }
    segments
}

/// What a run of `name` calls is CALLED. Every shell-ish tool folds into the
/// reference's "shell commands"; anything else folds into its own name.
#[must_use]
pub fn fold_noun(name: &str, count: usize) -> String {
    let plural = count != 1;
    if matches!(
        name,
        "bash" | "shell" | "sh" | "zsh" | "process_exec" | "ssh_shell" | "command"
    ) {
        return if plural {
            "shell commands".to_owned()
        } else {
            "shell command".to_owned()
        };
    }
    if plural {
        format!("{name} calls")
    } else {
        format!("{name} call")
    }
}

/// The affordance closing a bounded expanded region: what it is still
/// hiding, how to walk it, and how to open the lot.
///
/// Verify 1 (F2): the bounded window had no way to reach rows 10+ short of
/// opening everything, so the footer now names the page keys too. `above`
/// is what the internal scroll has already passed.
#[must_use]
pub fn show_all_segments(above: usize, below: usize) -> Vec<Segment> {
    let mut segments = vec![Segment::new("    └ ", Tone::Structure)];
    let hidden = above + below;
    segments.push(Segment::new(
        format!("⋯ {hidden} more row{}", if hidden == 1 { "" } else { "s" }),
        Tone::Meta,
    ));
    if above > 0 {
        segments.push(Segment::new(format!(" ({above} above)"), Tone::Meta));
    }
    if below > 0 {
        segments.push(Segment::new(" · ⇟/⇞ page", Tone::Accent));
    }
    segments.push(Segment::new(" · ⏎ show all", Tone::Accent));
    segments
}

/// Retained output as DISPLAY rows: every logical line hard-wrapped to
/// `width` cells, so no suffix is unreachable (verify 1, F2 — `ellipsize`
/// silently dropped the tail of every long line, `show all` included).
///
/// Expanded views retain even the leading mid-line fragment left by the
/// output cap. The renderer labels truncation separately; only collapsed
/// summaries skip that fragment. Wrap at grapheme boundaries using terminal
/// cells, so wide glyphs cannot make a bounded page occupy extra rows.
#[must_use]
pub fn output_rows(output: &str, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for line in output.lines() {
        if width == 0 {
            rows.push(line.to_owned());
            continue;
        }
        let mut row = String::new();
        let mut cells = 0;
        for grapheme in line.graphemes(true) {
            // A glyph wider than the entire viewport cannot be drawn.
            // Keep a visible replacement at that position until resize.
            let grapheme = if grapheme.width() > width {
                "�"
            } else {
                grapheme
            };
            let next = grapheme.width();
            if cells + next > width {
                rows.push(std::mem::take(&mut row));
                cells = 0;
            }
            row.push_str(grapheme);
            cells += next;
        }
        // Blank retained lines are rows too.
        rows.push(row);
    }
    rows
}

/// The bounded window over `rows`, and what it leaves above and below.
/// The offset is clamped here, so a stale scroll can never show an empty
/// window.
#[must_use]
pub fn bounded_window(rows: usize, scroll: usize, budget: usize) -> (usize, usize, usize) {
    if budget == 0 || rows == 0 {
        return (0, 0, rows);
    }
    let max_scroll = rows.saturating_sub(budget);
    let start = scroll.min(max_scroll);
    let end = start.saturating_add(budget).min(rows);
    (start, end, rows - end)
}

/// Char-truncate with a trailing ellipsis (the transcript's shared
/// `text-overflow: ellipsis`).
#[must_use]
pub fn ellipsize(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if text.chars().count() <= budget {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// The first line worth showing: non-blank, and not a pure rule of box or
/// dash glyphs (a `────` banner answers nothing about what the tool did).
#[must_use]
pub fn first_meaningful_line(output: &str) -> Option<&str> {
    output.lines().map(str::trim).find(|line| {
        !line.is_empty()
            && !line
                .chars()
                .all(|c| matches!(c, '-' | '=' | '_' | '─' | '━' | '═' | '*' | '#' | '.'))
    })
}

// -------------------------------------------------- colour by MEANING ---

/// Split one output line into meaning-toned segments (owner item 3). The
/// scan is a single pass over whitespace-delimited tokens, because it runs
/// per visible output row per dirty frame.
///
/// What earns its own ink: a NON-ZERO exit code, the verdict vocabulary
/// (`SHIP` · `HOLD` · `NO-SHIP` · `FAIL` · `ERROR`), a `file:line[:col]`
/// coordinate, and a thread/run/commit id. Everything else is metadata ink —
/// which, since this wave, clears 4.5:1 on every ground.
#[must_use]
pub fn meaning_segments(line: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut rest = line;
    // `exit`/`rc`/`code` colour the NUMBER that follows them, so the tone
    // has to survive one token.
    let mut exit_pending = false;
    while !rest.is_empty() {
        let lead = rest.len() - rest.trim_start().len();
        if lead > 0 {
            push_toned(&mut out, &rest[..lead], Tone::Meta);
            rest = &rest[lead..];
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (token, tail) = rest.split_at(end);
        let tone = if exit_pending && is_exit_value(token) {
            if token.trim_matches(|c: char| !c.is_ascii_digit() && c != '-') == "0" {
                Tone::Meta
            } else {
                Tone::Err
            }
        } else {
            token_tone(token)
        };
        exit_pending = introduces_exit(token);
        push_toned(&mut out, token, tone);
        rest = tail;
    }
    if out.is_empty() {
        out.push(Segment::new(line.to_owned(), Tone::Meta));
    }
    out
}

fn push_toned(out: &mut Vec<Segment>, text: &str, tone: Tone) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.tone == tone
    {
        last.text.push_str(text);
        return;
    }
    out.push(Segment::new(text.to_owned(), tone));
}

/// `exit`, `rc=`, `code` — a token whose NEXT token is an exit value.
fn introduces_exit(token: &str) -> bool {
    let bare = token.trim_end_matches([':', '=']).to_ascii_lowercase();
    matches!(bare.as_str(), "exit" | "rc" | "status" | "code" | "exited")
}

fn is_exit_value(token: &str) -> bool {
    let bare = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '-');
    !bare.is_empty() && bare.parse::<i64>().is_ok()
}

fn token_tone(token: &str) -> Tone {
    let bare = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
    // Verdicts are matched EXACTLY as written: lowercase "ship" is English
    // prose, `SHIP` is a verdict. `FAIL`/`ERROR` also answer to their
    // ordinary lowercase spellings, which is how tools actually print them.
    match bare {
        "SHIP" | "PASS" | "PASSED" | "OK" => return Tone::Ok,
        "HOLD" | "SKIP" | "SKIPPED" | "WARN" | "WARNING" => return Tone::Warn,
        "NO-SHIP" | "NOSHIP" | "FAIL" | "FAILED" | "ERROR" | "FATAL" | "PANIC" => {
            return Tone::Err;
        }
        _ => {}
    }
    if let Some(exit) = bare.strip_prefix("rc=") {
        return if exit == "0" { Tone::Meta } else { Tone::Err };
    }
    let lower = bare.to_ascii_lowercase();
    match lower.as_str() {
        "error" | "errors" | "failed" | "failure" | "failures" | "fatal" | "panic" | "panicked" => {
            return Tone::Err;
        }
        "warning" | "warnings" | "warn" => return Tone::Warn,
        "ok" | "passed" | "success" | "succeeded" | "done" => return Tone::Ok,
        _ => {}
    }
    if is_file_line(bare) {
        return Tone::Accent;
    }
    if is_identifier(bare) {
        return Tone::Name;
    }
    Tone::Meta
}

/// `src/app.rs:5182` / `render.rs:6117:9` — a path-ish head followed by one
/// or two all-digit segments.
fn is_file_line(token: &str) -> bool {
    let mut parts = token.split(':');
    let Some(head) = parts.next() else {
        return false;
    };
    if head.is_empty() || !(head.contains('/') || head.contains('.')) {
        return false;
    }
    let digits: Vec<&str> = parts.collect();
    if digits.is_empty() || digits.len() > 2 {
        return false;
    }
    digits
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// A thread / run / agent / commit id: a `kind_xxxxxx` handle, or a bare
/// hex word long enough that it cannot be an ordinary English word.
fn is_identifier(token: &str) -> bool {
    if let Some((kind, rest)) = token.split_once('_')
        && kind.len() >= 2
        && kind.bytes().all(|b| b.is_ascii_lowercase())
        && rest.len() >= 6
        && rest.bytes().all(|b| b.is_ascii_alphanumeric())
        && rest.bytes().any(|b| b.is_ascii_digit())
    {
        return true;
    }
    token.len() >= 7
        && token.len() <= 40
        && token.bytes().all(|b| b.is_ascii_hexdigit())
        && token.bytes().any(|b| b.is_ascii_digit())
        && token.bytes().any(|b| b.is_ascii_alphabetic())
}

// ------------------------------------------------------------- fold runs ---

/// Where one entry sits in a folded run of consecutive same-tool calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FoldRole<'a> {
    /// Not part of any fold — render the row normally.
    Alone,
    /// The run's first entry: it renders `Ran N …` and nothing else.
    Head(&'a FoldRun),
    /// A member the head speaks for: it renders NO rows at all. Its measured
    /// height is zero, which the layout cache's correction machinery already
    /// handles (a run of five calls becomes two rows, not ten).
    Member,
}

/// One foldable run: which entries, the tool they share, and the sub-line
/// the fold row speaks with. `subline` is the LATEST member's first
/// meaningful result line — the reference's `Ran 2 shell commands` / `└
/// Resuming agent a830387` pair, where the elbow answers "and where did that
/// leave things?" rather than repeating the oldest call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FoldRun {
    pub start: usize,
    pub len: usize,
    pub name: String,
    pub subline: Option<String>,
    /// Any member's retained output tail lost earlier output to the cap.
    pub truncated: bool,
    /// Any member had an output chunk the client could not decode.
    pub decode_error: bool,
}

impl FoldRun {
    /// The last entry index the run covers.
    #[must_use]
    pub const fn last(&self) -> usize {
        self.start + self.len - 1
    }
}

/// Classify `index` against a run scan. `runs` must be sorted and disjoint;
/// `render.rs` builds it once per frame from the projection.
#[must_use]
pub fn fold_role<'a>(runs: &'a [FoldRun], index: usize) -> FoldRole<'a> {
    let Some(run) = runs
        .iter()
        .find(|run| index >= run.start && index < run.start + run.len)
    else {
        return FoldRole::Alone;
    };
    if index == run.start {
        FoldRole::Head(run)
    } else {
        FoldRole::Member
    }
}

/// Scan a transcript for foldable runs.
///
/// `probe(index)` answers `Some(tool_name)` for an entry that is a COLLAPSED,
/// settled, unfocused tool call and `None` for everything else — a streaming
/// row, an expanded row, the focused row, a user message, an agent reply. A
/// run therefore breaks on anything the reader is actually looking at, which
/// is why "one member of a fold is expanded but hidden" is not a state this
/// can produce: walking ⌥N into a folded member REVEALS it.
#[must_use]
pub fn scan_runs<F>(len: usize, mut probe: F) -> Vec<FoldRun>
where
    F: FnMut(usize) -> Option<String>,
{
    let mut runs: Vec<FoldRun> = Vec::new();
    let mut index = 0usize;
    while index < len {
        let Some(name) = probe(index) else {
            index += 1;
            continue;
        };
        let start = index;
        index += 1;
        while index < len && probe(index).as_deref() == Some(name.as_str()) {
            index += 1;
        }
        let run_len = index - start;
        if run_len >= FOLD_MIN {
            runs.push(FoldRun {
                start,
                len: run_len,
                name,
                subline: None,
                truncated: false,
                decode_error: false,
            });
        }
    }
    runs
}

// ----------------------------------------------------- argument summary ---

/// The argument summary shown inside a tool row's parentheses (the sim's
/// `ToolRow .desc`, tui.js:3901-3908). The turn engine carries desc/meta via
/// the args convention (`{"desc": …, "meta": …}` — §6); legacy scripts carry
/// path/query/glob; the CU-2 computer tool carries a structured action.
///
/// Moved out of `render.rs` in this wave so the row format is testable
/// without a frame.
#[must_use]
pub fn arg_summary(args: &serde_json::Value) -> String {
    if let Some(desc) = args.get("desc").and_then(|v| v.as_str()) {
        return desc.to_owned();
    }
    if let Some(action) = args.get("action").and_then(|v| v.as_str()) {
        return computer_action_summary(action, args);
    }
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        return path.to_owned();
    }
    if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
        return match args.get("glob").and_then(|v| v.as_str()) {
            Some(glob) => format!("\"{query}\" {glob}"),
            None => format!("\"{query}\""),
        };
    }
    String::new()
}

/// Build the compact, tool-aware description shown in a semantic row.
///
/// Tool adapters already carry the useful fields in their JSON arguments, so
/// this remains a renderer-only policy: no wire change and no second fold
/// mechanism.  The generic [`arg_summary`] is retained for callers that do
/// not know the tool name; transcript rows should prefer this function.
#[must_use]
pub fn semantic_summary(name: &str, args: &serde_json::Value) -> String {
    let text = |key: &str| args.get(key).and_then(serde_json::Value::as_str);
    let count = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            args.get(*key).and_then(|value| {
                value
                    .as_u64()
                    .map(|number| number.to_string())
                    .or_else(|| value.as_array().map(|items| items.len().to_string()))
            })
        })
    };
    let command = text("command").or_else(|| text("cmd"));
    let path = text("path")
        .or_else(|| text("file"))
        .or_else(|| text("file_path"));
    let query = text("query")
        .or_else(|| text("pattern"))
        .or_else(|| text("search"));
    let glob = text("glob").or_else(|| text("include"));
    let description = text("description").or_else(|| text("desc"));
    match name {
        "bash" | "shell" | "sh" | "zsh" | "process_exec" | "ssh_shell" | "command" => {
            match (command, description) {
                (Some(command), Some(description)) if command != description => {
                    format!("{command} — {description}")
                }
                (Some(command), _) | (_, Some(command)) => command.to_owned(),
                _ => arg_summary(args),
            }
        }
        "fs_read" | "read" | "read_file" | "file_read" => {
            let mut summary = path.map(str::to_owned).unwrap_or_else(|| arg_summary(args));
            if let Some(range) = text("line_range").or_else(|| text("lines")) {
                summary.push_str(" · ");
                summary.push_str(range);
            }
            summary
        }
        "fs_search" | "grep" | "ripgrep" | "search" | "file_search" => {
            let mut summary = match (query, glob, path) {
                (Some(query), Some(glob), _) => format!("\"{query}\" {glob}"),
                (Some(query), None, Some(path)) => format!("\"{query}\" in {path}"),
                (Some(query), None, None) => format!("\"{query}\""),
                _ => arg_summary(args),
            };
            if let Some(matches) = count(&["match_count", "matches", "count", "results"]) {
                summary.push_str(" · ");
                summary.push_str(&matches);
                summary.push_str(if matches == "1" { " match" } else { " matches" });
            }
            summary
        }
        "agent_spawn" | "task" | "task_spawn" | "subagent" => {
            let label = text("label").or_else(|| text("name")).or(description);
            let state = text("state").or_else(|| text("status"));
            match (label, state) {
                (Some(label), Some(state)) => format!("{label} · {state}"),
                (Some(label), None) => label.to_owned(),
                (None, Some(state)) => state.to_owned(),
                _ => arg_summary(args),
            }
        }
        "fs_edit" | "edit" | "write" | "file_edit" => {
            let mut summary = path.map(str::to_owned).unwrap_or_else(|| arg_summary(args));
            if let Some(operation) = text("operation").or_else(|| text("action"))
                && !operation.is_empty()
                && operation != "edit"
            {
                summary = format!("{operation} {summary}");
            }
            summary
        }
        _ => description
            .map(str::to_owned)
            .unwrap_or_else(|| arg_summary(args)),
    }
}

/// Human-readable summary of one CU-2 computer action. The transcript is the
/// owner's window into a session that can move their real cursor, so the row
/// renders exactly what the model is doing to the screen.
#[must_use]
pub fn computer_action_summary(action: &str, args: &serde_json::Value) -> String {
    let u = |key| args.get(key).and_then(serde_json::Value::as_u64);
    let xy = || match (u("x"), u("y")) {
        (Some(x), Some(y)) => format!(" ({x}, {y})"),
        _ => String::new(),
    };
    let point = |key| {
        args.get(key).map_or_else(String::new, |p| {
            match (
                p.get("x").and_then(serde_json::Value::as_u64),
                p.get("y").and_then(serde_json::Value::as_u64),
            ) {
                (Some(x), Some(y)) => format!("({x}, {y})"),
                _ => String::new(),
            }
        })
    };
    match action {
        "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click"
        | "mouse_move" => {
            format!("{action}{}", xy())
        }
        "left_click_drag" => format!("drag {} → {}", point("from"), point("to")),
        "type" => match args.get("text").and_then(|v| v.as_str()) {
            Some(text) => format!("type \"{text}\""),
            None => "type".to_owned(),
        },
        "key" => match args.get("keys").and_then(|v| v.as_str()) {
            Some(keys) => format!("key {keys}"),
            None => "key".to_owned(),
        },
        "scroll" => {
            let dir = args.get("direction").and_then(|v| v.as_str()).unwrap_or("");
            let amount = u("amount").unwrap_or(0);
            format!("scroll {dir} ×{amount}{}", xy())
        }
        "wait" => match u("ms") {
            Some(ms) => format!("wait {ms}ms"),
            None => "wait".to_owned(),
        },
        other => other.replace('_', " "),
    }
}
