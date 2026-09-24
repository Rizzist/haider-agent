//! Repeated-tool-call loop guard: the typed, non-terminal `loop_suspected_v1`
//! steer and the thresholds that lead to `loop_limit`.
//!
//! A call "repeats" when its tool name, canonical arguments and normalized
//! result (digits masked, whitespace/Unicode normalized; see
//! `haider-core`'s `continuation_fingerprint`) were already seen in the same
//! turn. Any new call fingerprint or new assistant text resets the streak.
//! This is not a request-count cap: productive calls never accumulate.

use crate::ids::RunId;
use crate::item::TurnItem;
use serde::{Deserialize, Serialize};

pub const LOOP_SUSPECTED_EXTENSION_KIND: &str = "loop_suspected_v1";

/// Consecutive repeated tool calls before the model receives the typed
/// `loop_suspected_v1` steer. Rationale: a productive agent rarely makes 30
/// calls in a row that each return exactly what an identical earlier call
/// already returned, with no new call, result or text in between; polling a
/// slow job still has 30 identical probes of headroom before any steer.
pub const REPEATED_CALLS_BEFORE_LOOP_SUSPECTED: usize = 30;

/// Further consecutive repeated tool calls after the steer before the turn
/// ends in `loop_limit` (CLI exit 70). Rationale: the model gets as long to
/// react to the steer as it had to enter the loop; 60 exact repeats stay
/// close to the retired 64-request default only for this pathological case.
pub const REPEATED_CALLS_AFTER_SUSPECTED_BEFORE_LOOP_LIMIT: usize = 30;

/// Thresholds for the repeated-tool-call guard. Embedders may disable the
/// guard (`None` in the harness configuration) or choose other thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLoopGuardV1 {
    /// Consecutive repeated calls before the steer.
    pub suspect_after: usize,
    /// Further consecutive repeated calls after the steer before `loop_limit`.
    pub stop_after_suspected: usize,
}

impl Default for ToolLoopGuardV1 {
    fn default() -> Self {
        Self {
            suspect_after: REPEATED_CALLS_BEFORE_LOOP_SUSPECTED,
            stop_after_suspected: REPEATED_CALLS_AFTER_SUSPECTED_BEFORE_LOOP_LIMIT,
        }
    }
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
}

impl LoopSuspectedV1 {
    #[must_use]
    pub fn summary(&self) -> String {
        let tool = self
            .tool
            .as_deref()
            .map_or_else(String::new, |tool| format!(" (last: {tool})"));
        format!(
            "loop suspected — {} consecutive tool calls repeated earlier calls and results{tool}; the turn stops after {} more without progress",
            self.repeated_calls, self.stop_after
        )
    }

    /// Typed JSON wrapped in an explicit instruction for provider messages.
    #[must_use]
    pub fn model_note(&self) -> String {
        format!(
            "[loop_suspected_v1]\n{}\nYour last {} tool calls repeated calls you already made in this turn and returned the same results (ignoring numbers and whitespace). Change approach: use different tools or arguments, explain in text what is blocking you, or finish the turn. After {} more repeated calls without a new call, result, or text, this turn stops with loop_limit. This note applies only to run_id.\n[/loop_suspected_v1]",
            serde_json::json!({
                "type": LOOP_SUSPECTED_EXTENSION_KIND,
                "run_id": self.run_id,
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
        assert_eq!(
            ToolLoopGuardV1::default(),
            ToolLoopGuardV1 {
                suspect_after: 30,
                stop_after_suspected: 30
            }
        );
    }
}
