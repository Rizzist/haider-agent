//! Tool-loop guards: the typed, non-terminal `loop_suspected_v1` steer, the
//! thresholds that lead to `loop_limit`, and the typed `loop_limit` details.
//!
//! Two independent streaks are kept per turn (see `haider-core`'s
//! `ContinuationProgress` and `continuation_fingerprint`):
//!
//! - **Repeated tool calls** (result level): consecutive calls whose tool
//!   name, canonical arguments *and* normalized result (free-standing digit
//!   runs masked, whitespace/Unicode normalized) were already seen in the
//!   same turn. Only a new call fingerprint resets it.
//! - **Repeated actions** (action level, result independent): consecutive
//!   calls whose (tool, canonical arguments) were already seen in the turn,
//!   with no new (tool, arguments) in between. Only a new (tool, arguments)
//!   resets it; a changing result does not. It bounds an identical call whose
//!   result keeps changing only in letters (etags, request IDs, nonces).
//!   Computer-use/mobile-use screen steps (screenshot, UI tree, swipe,
//!   scroll, tap, key on the registered `computer`/`mobile` tools) are not
//!   counted while the observed screen changes, so paging through changing
//!   content is never capped; identical screens stay under the result guard.
//!
//! Assistant text never resets either streak: narration is not an action.
//! This is not a request-count cap: productive calls never accumulate.

use crate::ids::RunId;
use crate::item::TurnItem;
use serde::{Deserialize, Serialize};

pub const LOOP_SUSPECTED_EXTENSION_KIND: &str = "loop_suspected_v1";

/// Consecutive repeated tool calls before the model receives the typed
/// `loop_suspected_v1` steer. Rationale: a productive agent rarely makes 30
/// calls in a row that each return exactly what an identical earlier call
/// already returned, with no new call or result in between; polling a slow
/// job still has 30 identical probes of headroom before any steer.
pub const REPEATED_CALLS_BEFORE_LOOP_SUSPECTED: usize = 30;

/// Further consecutive repeated tool calls after the steer before the turn
/// ends in `loop_limit` (CLI exit 70). Rationale: the model gets as long to
/// react to the steer as it had to enter the loop; 60 exact repeats stay
/// close to the retired 64-request default only for this pathological case.
pub const REPEATED_CALLS_AFTER_SUSPECTED_BEFORE_LOOP_LIMIT: usize = 30;

/// Consecutive calls repeating an already-seen (tool, arguments) pair, with
/// no new pair in between, before the `loop_suspected_v1` steer, whatever
/// the results were. Rationale: generous enough that real polling of a
/// changing resource with one identical call (a growing build log polled 150
/// times) keeps going past the steer; waiting longer than that belongs to
/// background tasks and `monitor` watches, not to a repeated call.
pub const REPEATED_ACTIONS_BEFORE_LOOP_SUSPECTED: usize = 100;

/// Further consecutive repeated (tool, arguments) calls after the action
/// steer before `loop_limit`. Rationale: 200 identical calls without a single
/// new call is far beyond any productive poll, yet still bounded when the
/// result carries letter-bearing noise the result-level guard cannot mask.
pub const REPEATED_ACTIONS_AFTER_SUSPECTED_BEFORE_LOOP_LIMIT: usize = 100;

const fn default_action_suspect_after() -> usize {
    REPEATED_ACTIONS_BEFORE_LOOP_SUSPECTED
}

const fn default_action_stop_after_suspected() -> usize {
    REPEATED_ACTIONS_AFTER_SUSPECTED_BEFORE_LOOP_LIMIT
}

/// Thresholds for both tool-loop guards. Embedders may disable the guards
/// (`None` in the harness configuration) or choose other thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLoopGuardV1 {
    /// Consecutive repeated calls (same call and result) before the steer.
    pub suspect_after: usize,
    /// Further consecutive repeated calls after the steer before `loop_limit`.
    pub stop_after_suspected: usize,
    /// Consecutive repeated (tool, arguments) calls before the action steer.
    #[serde(default = "default_action_suspect_after")]
    pub action_suspect_after: usize,
    /// Further repeated (tool, arguments) calls after that steer before
    /// `loop_limit`.
    #[serde(default = "default_action_stop_after_suspected")]
    pub action_stop_after_suspected: usize,
}

impl Default for ToolLoopGuardV1 {
    fn default() -> Self {
        Self {
            suspect_after: REPEATED_CALLS_BEFORE_LOOP_SUSPECTED,
            stop_after_suspected: REPEATED_CALLS_AFTER_SUSPECTED_BEFORE_LOOP_LIMIT,
            action_suspect_after: REPEATED_ACTIONS_BEFORE_LOOP_SUSPECTED,
            action_stop_after_suspected: REPEATED_ACTIONS_AFTER_SUSPECTED_BEFORE_LOOP_LIMIT,
        }
    }
}

/// Which streak a `loop_suspected_v1` steer or a `loop_limit` refers to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopGuardKindV1 {
    /// Same (tool, arguments) and same normalized result. Journals written
    /// before the action guard existed carry no `guard` and mean this.
    #[default]
    RepeatedToolCalls,
    /// Same (tool, arguments), whatever the result.
    RepeatedActions,
    /// A guard kind from a newer writer.
    #[serde(other)]
    Unknown,
}

/// Typed `loop_limit` details, carried as `HaiderError::details` and as
/// `ErrorPresentation::loop_limit` (so `run_failed` and the `haider.run.v1`
/// `error.presentation` expose them). Tagged by `loop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "loop", rename_all = "snake_case")]
pub enum LoopLimitV1 {
    /// Consecutive MaxTokens/PauseTurn continuations without progress.
    NoProgressContinuations {
        continuation_count: usize,
        continuation_limit: usize,
    },
    /// Consecutive calls repeating an earlier call and its result.
    RepeatedToolCalls {
        repeated_calls: usize,
        suspect_after: usize,
        stop_after_suspected: usize,
    },
    /// Consecutive calls repeating an earlier (tool, arguments) pair.
    RepeatedActions {
        repeated_calls: usize,
        suspect_after: usize,
        stop_after_suspected: usize,
    },
}

/// Durable, UI-visible and model-visible steer. It never ends the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopSuspectedV1 {
    /// The turn whose calls repeated.
    pub run_id: RunId,
    /// Consecutive repeated calls observed when the steer was issued.
    pub repeated_calls: usize,
    /// Further repeated calls without progress before `loop_limit`.
    pub stop_after: usize,
    /// Tool name of the most recent repeated call, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The streak that triggered the steer.
    #[serde(default)]
    pub guard: LoopGuardKindV1,
}

impl LoopSuspectedV1 {
    #[must_use]
    pub fn summary(&self) -> String {
        let tool = self
            .tool
            .as_deref()
            .map_or_else(String::new, |tool| format!(" (last: {tool})"));
        let (what, reset) = match self.guard {
            LoopGuardKindV1::RepeatedActions => (
                "repeated earlier calls with the same arguments",
                "a new call",
            ),
            LoopGuardKindV1::RepeatedToolCalls | LoopGuardKindV1::Unknown => {
                ("repeated earlier calls and results", "a new call or result")
            }
        };
        format!(
            "loop suspected — {} consecutive tool calls {what}{tool}; the turn stops after {} more without {reset}",
            self.repeated_calls, self.stop_after
        )
    }

    /// Typed JSON wrapped in an explicit instruction for provider messages.
    /// It steers toward a different action or a wait tool; writing text does
    /// not reset the streak, so the note never suggests narrating.
    #[must_use]
    pub fn model_note(&self) -> String {
        let (what, reset) = match self.guard {
            LoopGuardKindV1::RepeatedActions => (
                "used a tool with the same arguments as an earlier call in this turn, with no new call in between (whatever the results were)",
                "a new call",
            ),
            LoopGuardKindV1::RepeatedToolCalls | LoopGuardKindV1::Unknown => (
                "repeated calls you already made in this turn and returned the same results (ignoring numbers and whitespace)",
                "a new call or a new result",
            ),
        };
        format!(
            "[loop_suspected_v1]\n{}\nYour last {} tool calls {what}. Writing more text does not change this count; only a different action does. Change approach now: call a different tool or use different arguments. If you are waiting for something slow, do not poll it with the same call: run it as a background task or use a wait/monitor tool (for example `monitor`) when one is available. If you cannot make progress, finish the turn and state what is blocking you. After {} more repeated calls without {reset}, this turn stops with loop_limit. This note applies only to run_id.\n[/loop_suspected_v1]",
            serde_json::json!({
                "type": LOOP_SUSPECTED_EXTENSION_KIND,
                "run_id": self.run_id,
                "guard": self.guard,
                "repeated_calls": self.repeated_calls,
                "stop_after": self.stop_after,
                "tool": self.tool,
            }),
            self.repeated_calls,
            self.stop_after,
        )
    }

    pub fn to_extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        let mut data = serde_json::to_value(self)?;
        data["label"] = serde_json::Value::String(self.summary());
        Ok(TurnItem::Extension {
            kind: LOOP_SUSPECTED_EXTENSION_KIND.into(),
            data,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        if kind != LOOP_SUSPECTED_EXTENSION_KIND {
            return None;
        }
        serde_json::from_value(data.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn steer_round_trips_as_a_typed_extension_and_model_note() {
        let note = LoopSuspectedV1 {
            run_id: RunId::new("run-1"),
            repeated_calls: 30,
            stop_after: 30,
            tool: Some("fs_read".into()),
            guard: LoopGuardKindV1::RepeatedToolCalls,
        };
        let item = note.to_extension_item().expect("item");
        let TurnItem::Extension { kind, data } = &item else {
            panic!("extension item");
        };
        assert_eq!(kind, LOOP_SUSPECTED_EXTENSION_KIND);
        assert_eq!(data["label"], note.summary());
        assert_eq!(
            LoopSuspectedV1::from_extension_item(&item),
            Some(note.clone())
        );
        let text = note.model_note();
        assert!(text.starts_with("[loop_suspected_v1]\n{"));
        assert!(text.ends_with("[/loop_suspected_v1]"));
        assert!(text.contains(r#""repeated_calls":30"#));
        assert!(text.contains(r#""guard":"repeated_tool_calls""#));
        // The steer must not invite a text-only escape.
        assert!(!text.contains("explain in text"));
        assert!(text.contains("does not change this count"));
        assert!(text.contains("monitor"));
        assert!(text.contains("without a new call or a new result"));
        assert!(note.summary().ends_with("without a new call or result"));
        assert_eq!(
            ToolLoopGuardV1::default(),
            ToolLoopGuardV1 {
                suspect_after: 30,
                stop_after_suspected: 30,
                action_suspect_after: 100,
                action_stop_after_suspected: 100,
            }
        );
    }

    #[test]
    fn journals_without_guard_kind_read_as_repeated_tool_calls() {
        let item = TurnItem::Extension {
            kind: LOOP_SUSPECTED_EXTENSION_KIND.into(),
            data: serde_json::json!({
                "run_id": "run-old",
                "repeated_calls": 30,
                "stop_after": 30,
                "tool": "fs_read",
                "label": "old label",
            }),
        };
        let note = LoopSuspectedV1::from_extension_item(&item).expect("legacy steer");
        assert_eq!(note.guard, LoopGuardKindV1::RepeatedToolCalls);
        let future: LoopSuspectedV1 = serde_json::from_value(serde_json::json!({
            "run_id": "run-new",
            "repeated_calls": 1,
            "stop_after": 1,
            "guard": "future_guard",
        }))
        .expect("unknown guard kind is tolerated");
        assert_eq!(future.guard, LoopGuardKindV1::Unknown);
        let config: ToolLoopGuardV1 = serde_json::from_value(
            serde_json::json!({"suspect_after": 5, "stop_after_suspected": 6}),
        )
        .expect("older config");
        assert_eq!(config.action_suspect_after, 100);
        assert_eq!(config.action_stop_after_suspected, 100);
    }

    #[test]
    fn action_steer_names_its_guard_and_loop_limit_details_are_tagged() {
        let note = LoopSuspectedV1 {
            run_id: RunId::new("run-2"),
            repeated_calls: 100,
            stop_after: 100,
            tool: Some("fs_read".into()),
            guard: LoopGuardKindV1::RepeatedActions,
        };
        assert!(note.model_note().contains("same arguments"));
        assert!(note.summary().contains("same arguments"));
        assert!(note.summary().ends_with("without a new call"));
        assert_eq!(
            serde_json::to_value(LoopLimitV1::RepeatedActions {
                repeated_calls: 200,
                suspect_after: 100,
                stop_after_suspected: 100,
            })
            .expect("details"),
            serde_json::json!({
                "loop": "repeated_actions",
                "repeated_calls": 200,
                "suspect_after": 100,
                "stop_after_suspected": 100,
            })
        );
        assert_eq!(
            serde_json::to_value(LoopLimitV1::NoProgressContinuations {
                continuation_count: 9,
                continuation_limit: 8,
            })
            .expect("details"),
            serde_json::json!({
                "loop": "no_progress_continuations",
                "continuation_count": 9,
                "continuation_limit": 8,
            })
        );
    }
}
