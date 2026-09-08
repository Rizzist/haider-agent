//! The live background-task line under the composer (971-tui-collapse
//! addition, owner 2026-09-08 13:0xZ).
//!
//! Reference `state/reference/claude-code-status-line-tasks.png`: one line
//! under the composer reading `▸▸ bypass permissions on · 6 shells, 14
//! monitors`, which expands into the per-task list (`● main`, then one row
//! per background task with its type and its current one-line activity).
//! The owner's complaint: a Haider session reported "seven monitors, all
//! armed" in prose while the area under the composer said nothing.
//!
//! This module is the PURE half — counts, labels, row assembly — and it
//! borrows `toolfold`'s [`Segment`]/[`Tone`] vocabulary so the whole wave
//! speaks one language and `style.rs` stays the single colour seam.
//!
//! HONESTY BOUNDARY (what the daemon does and does not tell us today):
//!
//! * Counts — shells, monitors, subagents and background tasks — are all
//!   observed truth (`shell.list`, `monitor.list`, `AgentSpawned`, the
//!   `task_started`/`task_completed` facts).
//! * Elapsed is observed for every kind: `ShellWire::created_at_ms`,
//!   `MonitorRegistrationWire::created_at_ms`, `ChipModel::spawned_at_ms`,
//!   `TaskStarted::started_at_ms`.
//! * The ACTIVITY line exists for subagents (`ChipModel::activity`, derived
//!   from the child's mirrored transcript) and monitors
//!   (`MonitorRegistrationWire::last_event.summary` / `source_summary`).
//! * It does NOT exist for a background SHELL TASK. `TaskStarted` carries
//!   the command at spawn and nothing after it: there is no `task_progress`
//!   event and no `task.list` RPC, so the daemon's rolling task tail only
//!   reaches the client inside `TaskCompleted`. Those rows therefore show
//!   what the task IS (its command), never a fabricated "what it is doing".
//!   The exact additive field this needs is recorded in the lane report.
//! * There is likewise no structured agent-TYPE vocabulary on the wire (the
//!   reference's `general-purpose`). The only structured type is the Loom
//!   `@type ·` prefix on a typed spawn; everything else falls back to
//!   `subagent`.

use crate::toolfold::{Segment, Tone, ellipsize};

/// Task rows one PAGE of the expanded list shows before it yields to the
/// `+K more` row.
///
/// Verify 1 (F7): that row used to be inert, so a session with fourteen
/// monitors could not reveal them at all. It is an affordance now — click
/// it, or `/tasks more`, to walk the pages.
pub const MAX_ROWS: usize = 8;

/// What a row in the expanded list IS. The kind is the row's "type" column —
/// which collection the row came from is observed truth even where the wire
/// carries no type tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    /// The session itself — the reference's `● main`.
    Main,
    Agent,
    Shell,
    Monitor,
    Task,
}

impl RowKind {
    /// The glyph: a FILLED bullet for the session (it is the reader's own
    /// seat), a hollow one for everything running beside it.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Main => "●",
            _ => "○",
        }
    }

    /// The type word shown in the row's second column.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Agent => "subagent",
            Self::Shell => "shell",
            Self::Monitor => "monitor",
            Self::Task => "task",
        }
    }
}

/// One row of the expanded list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub kind: RowKind,
    /// The row's own type word — the Loom agent type where a typed spawn
    /// supplied one, else [`RowKind::label`].
    pub kind_label: String,
    /// What this task IS: the agent's delegated task, the shell's title, the
    /// monitor's source summary, the background task's name.
    pub name: String,
    /// What it is DOING right now, where the daemon actually tells us.
    /// `None` renders no activity column rather than a guess.
    pub activity: Option<String>,
    /// Milliseconds since it started, where a start instant was observed.
    pub elapsed_ms: Option<u64>,
}

/// The live counts behind the collapsed summary.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    pub shells: usize,
    pub monitors: usize,
    pub agents: usize,
    pub tasks: usize,
}

impl Counts {
    #[must_use]
    pub const fn total(self) -> usize {
        self.shells + self.monitors + self.agents + self.tasks
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.total() == 0
    }

    /// `6 shells, 14 monitors, 2 agents` — the reference's comma list. A
    /// ZERO count is omitted entirely (the `band_counts` law), so the line
    /// never reads `0 monitors`.
    #[must_use]
    pub fn text(self) -> String {
        let plural = |n: usize, one: &str, many: &str| {
            (n > 0).then(|| format!("{n} {}", if n == 1 { one } else { many }))
        };
        [
            plural(self.shells, "shell", "shells"),
            plural(self.monitors, "monitor", "monitors"),
            plural(self.agents, "agent", "agents"),
            plural(self.tasks, "task", "tasks"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<String>>()
        .join(", ")
    }
}

/// The session's effect-permission posture, as the daemon reported it.
///
/// This is DERIVED from `SessionMetadataV1::permission_overrides`, which the
/// daemon already sends on every session summary — the TUI simply used to
/// throw it away. It is never inferred from what the TUI itself asked for at
/// `session.create`: an unreported posture renders NO segment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PermissionMode {
    /// `auto_allow` — the Codex `--full-auto` analogue.
    BypassAll,
    /// Writes and exec pre-allowed, other classes still ask.
    WritesAndExec,
    /// `read_only` — model-initiated writes denied outright.
    ReadOnly,
    /// Every class goes through the permission menu.
    Approvals,
}

impl PermissionMode {
    /// Read the posture off the wire value. `read_only` takes precedence
    /// over every allow field, exactly as the daemon applies it.
    #[must_use]
    pub const fn from_overrides(
        overrides: &haider_protocol::session::SessionPermissionOverridesV1,
    ) -> Self {
        if overrides.read_only {
            Self::ReadOnly
        } else if overrides.auto_allow {
            Self::BypassAll
        } else if overrides.allow_writes && overrides.allow_exec {
            Self::WritesAndExec
        } else {
            Self::Approvals
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BypassAll => "bypass permissions on",
            Self::WritesAndExec => "writes + exec allowed",
            Self::ReadOnly => "read-only",
            Self::Approvals => "approvals on",
        }
    }

    /// The posture's tone: a lifted gate is the accent (the reference paints
    /// it), a denied-writes session is a warning, an asking session is quiet.
    #[must_use]
    pub const fn tone(self) -> Tone {
        match self {
            Self::BypassAll => Tone::Accent,
            Self::WritesAndExec => Tone::Accent,
            Self::ReadOnly => Tone::Warn,
            Self::Approvals => Tone::Meta,
        }
    }
}

/// Everything the line renders from, gathered once per frame.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StatusLine {
    pub mode: Option<PermissionMode>,
    pub counts: Counts,
    pub rows: Vec<Row>,
    pub expanded: bool,
    /// Which page of the list is showing (F7). Clamped by [`Self::page`],
    /// so a page that a finished task emptied falls back to the first.
    pub page: usize,
}

impl StatusLine {
    /// True when the line has something to say. With no posture reported and
    /// nothing running there is nothing to show, and the row is not spent.
    #[must_use]
    pub fn shows(&self) -> bool {
        self.mode.is_some() || !self.counts.is_empty()
    }

    /// Pages the list spans — at least one, so `page 1 of 1` is sayable.
    #[must_use]
    pub fn pages(&self) -> usize {
        self.rows.len().div_ceil(MAX_ROWS).max(1)
    }

    /// The page actually showing, clamped into range: a page a finished
    /// task emptied falls back to the first rather than rendering nothing.
    #[must_use]
    pub fn page(&self) -> usize {
        self.page.min(self.pages() - 1)
    }

    /// Rows this line occupies: one collapsed; expanded, the summary plus
    /// this page of the task list plus the `+K more` affordance when the
    /// list spans more than one page.
    #[must_use]
    pub fn height(&self) -> u16 {
        if !self.shows() {
            return 0;
        }
        if !self.expanded {
            return 1;
        }
        let (listed, hidden) = self.listed();
        let overflow = usize::from(hidden > 0);
        u16::try_from(1 + listed.len() + overflow).unwrap_or(u16::MAX)
    }

    /// The collapsed summary: `▸▸ bypass permissions on · 6 shells, 14 monitors`.
    /// The chevrons are the expand affordance and flip when it opens.
    #[must_use]
    pub fn summary_segments(&self, width: usize) -> Vec<Segment> {
        let mut segments = vec![Segment::new(
            if self.expanded {
                " ▾▾ "
            } else {
                " ▸▸ "
            },
            Tone::Accent,
        )];
        if let Some(mode) = self.mode {
            segments.push(Segment::new(mode.label(), mode.tone()));
        }
        let counts = self.counts.text();
        if !counts.is_empty() {
            if segments.len() > 1 {
                segments.push(Segment::new(" · ", Tone::Structure));
            }
            segments.push(Segment::new(counts, Tone::Meta));
        }
        if width > 0 {
            truncate_segments(&mut segments, width);
        }
        segments
    }

    /// One task row: `  ○ subagent  Checking the exit-code gate · 4m 12s`.
    ///
    /// The type word and the elapsed figure are fixed; the ACTIVITY (or, for
    /// a row the daemon reports no activity for, the name) is the elastic
    /// column — the same budgeting law the collapsed tool row uses.
    #[must_use]
    pub fn row_segments(row: &Row, width: usize) -> Vec<Segment> {
        let mut head = vec![
            Segment::new("  ", Tone::Structure),
            Segment::new(
                format!("{} ", row.kind.glyph()),
                match row.kind {
                    RowKind::Main => Tone::Accent,
                    _ => Tone::Meta,
                },
            ),
            Segment::new(row.kind_label.clone(), Tone::Name),
        ];
        let mut tail: Vec<Segment> = Vec::new();
        if let Some(ms) = row.elapsed_ms {
            tail.push(Segment::new(
                format!(" · {}", crate::format::fmt_elapsed(ms)),
                Tone::Meta,
            ));
        }
        // The activity is what it is DOING; the name is what it IS. A row
        // with no reported activity shows the name — never an invented verb.
        let detail = row.activity.clone().unwrap_or_else(|| row.name.clone());
        if !detail.is_empty() {
            let budget = if width == 0 {
                detail.chars().count()
            } else {
                width
                    .saturating_sub(crate::toolfold::segments_width(&head))
                    .saturating_sub(crate::toolfold::segments_width(&tail))
                    .saturating_sub(2)
            };
            if budget > 0 {
                head.push(Segment::new(
                    format!("  {}", ellipsize(&detail, budget)),
                    if row.activity.is_some() {
                        Tone::Body
                    } else {
                        Tone::Meta
                    },
                ));
            }
        }
        head.extend(tail);
        head
    }

    /// The `+K more` row closing an overflowing list — an AFFORDANCE, not
    /// a dead label (F7): it names the page it is on and the gesture that
    /// walks to the next, and wraps back to the first at the end.
    #[must_use]
    pub fn overflow_segments(&self, hidden: usize) -> Vec<Segment> {
        let (page, pages) = (self.page(), self.pages());
        let last = page + 1 == pages;
        vec![
            Segment::new("  ", Tone::Structure),
            Segment::new(format!("  +{hidden} more"), Tone::Meta),
            Segment::new(format!(" · page {} of {pages}", page + 1), Tone::Meta),
            Segment::new(
                if last {
                    " · ⏎ back to the first"
                } else {
                    " · ⏎ next page"
                },
                Tone::Accent,
            ),
        ]
    }

    /// This page's rows, and how many the page is not showing.
    #[must_use]
    pub fn listed(&self) -> (&[Row], usize) {
        if self.rows.is_empty() {
            return (&self.rows, 0);
        }
        let start = self.page() * MAX_ROWS;
        let end = start.saturating_add(MAX_ROWS).min(self.rows.len());
        (&self.rows[start..end], self.rows.len() - (end - start))
    }
}

/// Trim a segment row to `width` cells, ellipsizing the LAST segment that
/// still has room — the styled-row rule (`ellipsize_spans`): cutting through
/// a segment keeps its tone rather than flattening the row.
fn truncate_segments(segments: &mut Vec<Segment>, width: usize) {
    if crate::toolfold::segments_width(segments) <= width {
        return;
    }
    let mut used = 0usize;
    let mut kept: Vec<Segment> = Vec::new();
    for segment in segments.drain(..) {
        let cells = segment.width();
        if used + cells <= width.saturating_sub(1) {
            used += cells;
            kept.push(segment);
            continue;
        }
        let room = width.saturating_sub(1).saturating_sub(used);
        if room > 0 {
            kept.push(Segment::new(
                segment.text.chars().take(room).collect::<String>(),
                segment.tone,
            ));
        }
        kept.push(Segment::new("…", Tone::Meta));
        break;
    }
    *segments = kept;
}
