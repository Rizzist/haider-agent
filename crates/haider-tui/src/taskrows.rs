//! W-A background task rows: the session-scoped projection of
//! `task_started` / `task_completed` journal facts into (a) the live band
//! line above the composer and (b) the transcript's ambient note rows.
//!
//! Tasks are SESSION-scoped by runtime law (they outlive turns and die with
//! the session), so the panel travels whole with the session at
//! checkout/checkin — unlike chips it is never split per branch; only the
//! NOTE lands on the branch timeline the fact was stamped with.

use haider_protocol::task::{TaskCompleted, TaskEventPayload, TaskStarted, TaskTerminalState};

/// Bound applied to the note's command/tail excerpt (display only).
const NOTE_EXCERPT_CHARS: usize = 80;
/// Live-line name budget before the band collapses to a count.
const LINE_NAMED_TASKS: usize = 3;

/// One task's display state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    /// Opaque durable id (`task-…`).
    pub task: String,
    pub name: String,
    /// Journal time base (S4 law): the started fact's `started_at_ms`.
    pub started_at_ms: u64,
    pub state: TaskRowState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRowState {
    Running,
    Completed { exit_code: Option<i32> },
    Failed,
    Killed,
}

impl TaskRowState {
    #[must_use]
    pub fn is_running(&self) -> bool {
        *self == Self::Running
    }
}

/// The session's task list, in first-seen order.
#[derive(Debug, Clone, Default)]
pub struct TaskPanel {
    rows: Vec<TaskRow>,
}

impl TaskPanel {
    /// Applies one journal fact. Replay-shaped: a completed fact for an
    /// unseen task still lands a terminal row (the started fact may sit
    /// outside the replayed window), and duplicate facts are idempotent.
    pub fn apply(&mut self, fact: &TaskEventPayload) {
        match fact {
            TaskEventPayload::TaskStarted(started) => {
                if self.find(started.task.as_str()).is_none() {
                    self.rows.push(TaskRow {
                        task: started.task.as_str().to_owned(),
                        name: started.name.clone(),
                        started_at_ms: started.started_at_ms,
                        state: TaskRowState::Running,
                    });
                }
            }
            TaskEventPayload::TaskCompleted(completed) => {
                let state = match &completed.state {
                    TaskTerminalState::Completed { exit_code } => TaskRowState::Completed {
                        exit_code: *exit_code,
                    },
                    TaskTerminalState::Failed { .. } => TaskRowState::Failed,
                    TaskTerminalState::Killed => TaskRowState::Killed,
                };
                if let Some(row) = self.find_mut(completed.task.as_str()) {
                    row.state = state;
                } else {
                    self.rows.push(TaskRow {
                        task: completed.task.as_str().to_owned(),
                        name: completed.name.clone(),
                        started_at_ms: 0,
                        state,
                    });
                }
            }
        }
    }

    #[must_use]
    pub fn rows(&self) -> &[TaskRow] {
        &self.rows
    }

    #[must_use]
    pub fn running_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.state.is_running())
            .count()
    }

    fn find(&self, task: &str) -> Option<&TaskRow> {
        self.rows.iter().find(|row| row.task == task)
    }

    fn find_mut(&mut self, task: &str) -> Option<&mut TaskRow> {
        self.rows.iter_mut().find(|row| row.task == task)
    }
}

/// The transcript note one task fact becomes — the same dim ambient voice
/// as the subagent `report_note`/`messaged_note` rows, which also gives
/// plain mode its parity line for free.
#[must_use]
pub fn task_note(fact: &TaskEventPayload) -> String {
    match fact {
        TaskEventPayload::TaskStarted(started) => started_note(started),
        TaskEventPayload::TaskCompleted(completed) => completed_note(completed),
    }
}

fn started_note(fact: &TaskStarted) -> String {
    format!(
        "⚙ task started — {} · {}",
        fact.name,
        excerpt(&fact.command)
    )
}

fn completed_note(fact: &TaskCompleted) -> String {
    let disposition = match &fact.state {
        TaskTerminalState::Completed {
            exit_code: Some(code),
        } => format!("exit {code}"),
        TaskTerminalState::Completed { exit_code: None } => "exit (signal)".to_owned(),
        TaskTerminalState::Failed { .. } => "failed".to_owned(),
        TaskTerminalState::Killed => "killed".to_owned(),
    };
    let elapsed = crate::format::fmt_elapsed(fact.elapsed_ms);
    let tail = fact
        .tail
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(excerpt);
    match tail {
        Some(tail) => format!("└ task {} — {disposition} · {elapsed} — {tail}", fact.name),
        None => format!("└ task {} — {disposition} · {elapsed}", fact.name),
    }
}

fn excerpt(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let trimmed = flat.trim();
    if trimmed.chars().count() <= NOTE_EXCERPT_CHARS {
        trimmed.to_owned()
    } else {
        let mut cut: String = trimmed.chars().take(NOTE_EXCERPT_CHARS).collect();
        cut.push('…');
        cut
    }
}

/// The live band line above the composer: every RUNNING task with its
/// ticking elapsed (the S4 journal clock — `clock_ms` advances on the anim
/// tick and every applied envelope), collapsing to a count past the name
/// budget. `None` when nothing runs — the band sheds entirely.
#[must_use]
pub fn tasks_line(panel: &TaskPanel, clock_ms: u64) -> Option<String> {
    let running: Vec<&TaskRow> = panel
        .rows()
        .iter()
        .filter(|row| row.state.is_running())
        .collect();
    if running.is_empty() {
        return None;
    }
    let plural = if running.len() > 1 { "s" } else { "" };
    let mut segments = Vec::new();
    for row in running.iter().take(LINE_NAMED_TASKS) {
        let elapsed = crate::format::fmt_elapsed(clock_ms.saturating_sub(row.started_at_ms));
        segments.push(format!("{} {elapsed}", row.name));
    }
    if running.len() > LINE_NAMED_TASKS {
        segments.push(format!("+{} more", running.len() - LINE_NAMED_TASKS));
    }
    Some(format!(
        "⚙ {} background task{plural} — {}",
        running.len(),
        segments.join(" · ")
    ))
}
