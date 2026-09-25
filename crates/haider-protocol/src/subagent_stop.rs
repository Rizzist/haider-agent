//! Subagent stop semantics (owner decision 2026-09-25).
//!
//! Stopping the main thread stops only the main thread: its subagents keep
//! running, and a foreground subagent the stopped turn was waiting on
//! continues in the background with its result delivered to the session when
//! it finishes. Cancelling one named subagent stops only that subagent. An
//! explicit "stop all" stops every subagent in the tree, however deep.
//!
//! Two durable parent-session facts make the main-only stop honest:
//! [`SubagentsStillRunningV1`] records how many subagents the stop left
//! running, and [`DetachedChildResultV1`] delivers a detached foreground
//! subagent's result later. Both are `TurnItem::Extension` items.

use crate::agent::ReportVerification;
use crate::ids::{AgentId, RunId, SessionId};
use crate::item::TurnItem;
use serde::{Deserialize, Serialize};

pub const SUBAGENTS_STILL_RUNNING_EXTENSION_KIND: &str = "subagents_still_running_v1";
pub const DETACHED_CHILD_RESULT_EXTENSION_KIND: &str = "detached_child_result_v1";
/// Bounded number of named direct children carried by the marker.
pub const STILL_RUNNING_AGENT_LIST_MAX: usize = 16;
/// Bounded summary bytes carried by a delivered detached result.
pub const DETACHED_RESULT_SUMMARY_MAX_BYTES: usize = 16 * 1024;

/// CLI command that stops every subagent in a session's tree.
pub fn agent_cancel_all_command(parent_session_id: &SessionId) -> String {
    format!("haider agent cancel {parent_session_id} --all --json")
}

/// One direct child of the stopped turn that is still running or whose
/// result is still owed to the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StillRunningAgentV1 {
    pub agent: AgentId,
    pub task: String,
    /// The stopped turn was waiting on this child; its result is delivered
    /// to the session when it finishes.
    #[serde(default)]
    pub result_pending: bool,
}

/// Durable "N subagents still running" marker committed when a main-thread
/// stop leaves live subagents behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentsStillRunningV1 {
    pub parent_session_id: SessionId,
    pub stopped_run_id: RunId,
    /// Live subagents anywhere in the stopped turn's tree.
    pub still_running: u64,
    /// Direct children of the stopped turn (bounded).
    #[serde(default)]
    pub agents: Vec<StillRunningAgentV1>,
    #[serde(default)]
    pub agents_truncated: bool,
}

impl SubagentsStillRunningV1 {
    #[must_use]
    pub fn summary(&self) -> String {
        let noun = if self.still_running == 1 {
            "subagent"
        } else {
            "subagents"
        };
        let mut summary = format!(
            "main thread stopped — {} {noun} still running",
            self.still_running
        );
        let pending = self
            .agents
            .iter()
            .filter(|agent| agent.result_pending)
            .count();
        if pending > 0 {
            summary.push_str(&format!(
                " · {pending} waited-on result{} will be delivered when finished",
                if pending == 1 { "" } else { "s" }
            ));
        }
        summary.push_str(" · stop all: ");
        summary.push_str(&agent_cancel_all_command(&self.parent_session_id));
        summary
    }

    /// Bounded user-role note so the next turn knows its subagents live on.
    #[must_use]
    pub fn model_note(&self) -> String {
        let mut agents = self
            .agents
            .iter()
            .map(|agent| {
                format!(
                    "- {} ({}){}",
                    agent.agent,
                    agent.task,
                    if agent.result_pending {
                        " — its result will be delivered when it finishes"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>();
        if self.agents_truncated {
            agents.push("- …".into());
        }
        format!(
            "[subagents_still_running_v1]\nThe user stopped turn {} (main thread only). {} subagent(s) in its tree are still running. Its direct subagents:\n{}\nUse `haider agent cancel {} <agent> --json` to stop one subagent, or `{}` to stop all of them.\n[/subagents_still_running_v1]",
            self.stopped_run_id,
            self.still_running,
            agents.join("\n"),
            self.parent_session_id,
            agent_cancel_all_command(&self.parent_session_id),
        )
    }

    pub fn to_extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        let mut data = serde_json::to_value(self)?;
        data["label"] = serde_json::Value::String(self.summary());
        Ok(TurnItem::Extension {
            kind: SUBAGENTS_STILL_RUNNING_EXTENSION_KIND.into(),
            data,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        if kind != SUBAGENTS_STILL_RUNNING_EXTENSION_KIND {
            return None;
        }
        serde_json::from_value(data.clone()).ok()
    }
}

/// Terminal disposition of a detached foreground subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetachedChildStateV1 {
    Done,
    Errored,
    Cancelled,
}

/// A detached foreground subagent's result, delivered to its parent session
/// after the turn that was waiting on it was stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetachedChildResultV1 {
    pub agent: AgentId,
    pub task: String,
    pub stopped_run_id: RunId,
    pub state: DetachedChildStateV1,
    pub verified: ReportVerification,
    pub summary: String,
    #[serde(default)]
    pub summary_truncated: bool,
}

impl DetachedChildResultV1 {
    #[must_use]
    pub fn summary_line(&self) -> String {
        let state = match self.state {
            DetachedChildStateV1::Done => "finished",
            DetachedChildStateV1::Errored => "failed",
            DetachedChildStateV1::Cancelled => "was cancelled",
        };
        format!(
            "subagent {} ({}) {state} after its waiting turn was stopped — result delivered",
            self.agent, self.task
        )
    }

    /// The prompt copy of the delivered result (also the steer text).
    #[must_use]
    pub fn model_note(&self) -> String {
        let state = match self.state {
            DetachedChildStateV1::Done => "done",
            DetachedChildStateV1::Errored => "errored",
            DetachedChildStateV1::Cancelled => "cancelled",
        };
        format!(
            "[subagent result] agent {} ({}) — state: {state}. The turn that was waiting on it ({}) had been stopped, so its result is delivered here.{}\n{}",
            self.agent,
            self.task,
            self.stopped_run_id,
            if self.summary_truncated {
                " (summary truncated)"
            } else {
                ""
            },
            self.summary
        )
    }

    pub fn to_extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        let mut data = serde_json::to_value(self)?;
        data["label"] = serde_json::Value::String(self.summary_line());
        Ok(TurnItem::Extension {
            kind: DETACHED_CHILD_RESULT_EXTENSION_KIND.into(),
            data,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        if kind != DETACHED_CHILD_RESULT_EXTENSION_KIND {
            return None;
        }
        serde_json::from_value(data.clone()).ok()
    }
}

/// Bound a summary to `max` bytes on a char boundary.
#[must_use]
pub fn bounded_summary(summary: &str, max: usize) -> (String, bool) {
    if summary.len() <= max {
        return (summary.to_owned(), false);
    }
    let mut end = max;
    while end > 0 && !summary.is_char_boundary(end) {
        end -= 1;
    }
    (summary[..end].to_owned(), true)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_through_extension_item() {
        let marker = SubagentsStillRunningV1 {
            parent_session_id: SessionId::new("s"),
            stopped_run_id: RunId::new("r"),
            still_running: 2,
            agents: vec![StillRunningAgentV1 {
                agent: AgentId::new("a"),
                task: "t".into(),
                result_pending: true,
            }],
            agents_truncated: false,
        };
        let item = marker.to_extension_item().expect("encode");
        assert_eq!(
            SubagentsStillRunningV1::from_extension_item(&item),
            Some(marker.clone())
        );
        assert!(
            marker
                .summary()
                .starts_with("main thread stopped — 2 subagents still running")
        );
        assert!(
            marker
                .model_note()
                .contains("haider agent cancel s --all --json")
        );
    }

    #[test]
    fn detached_result_round_trips_and_bounds_summary() {
        let (summary, truncated) = bounded_summary("ééé", 3);
        assert_eq!((summary.as_str(), truncated), ("é", true));
        let result = DetachedChildResultV1 {
            agent: AgentId::new("a"),
            task: "t".into(),
            stopped_run_id: RunId::new("r"),
            state: DetachedChildStateV1::Done,
            verified: ReportVerification::Unverified,
            summary: "ok".into(),
            summary_truncated: false,
        };
        let item = result.to_extension_item().expect("encode");
        assert_eq!(
            DetachedChildResultV1::from_extension_item(&item),
            Some(result.clone())
        );
        assert!(result.model_note().contains("state: done"));
    }
}
