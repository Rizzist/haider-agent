//! The generic `plan` tool — review-before-commit for any deliverable.
//!
//! Like `request_input`, `plan` deliberately never enters the effect
//! permission broker: presenting a proposal is not a side effect. The owning
//! actor journals `MenuOpened`/`MenuAnswered`, parks the run in
//! `InputRequired`, and returns the decision as the tool result. What makes
//! it distinct from `request_input` is the payload: a full markdown document
//! (an architecture proposal, a migration plan, an agent-type design) carried
//! in the durable menu body, rendered by clients as a full-screen reviewable
//! surface with the fixed decision vocabulary accept / revise / reject.

use crate::request_input::RequestInputAnswer;
use crate::{ToolError, ToolResult};
use haider_protocol::ids::MenuId;
use haider_protocol::menu::{Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use serde::{Deserialize, Serialize};

/// Menu origin tag clients key their plan surfaces on.
pub const PLAN_ORIGIN: &str = "plan";
/// Title is a headline, not a document.
pub const PLAN_TITLE_MAX_BYTES: usize = 120;
/// The markdown body rides the durable menu; bounded like other durable text.
pub const PLAN_BODY_MAX_BYTES: usize = 32 * 1024;

pub const PLAN_DECISION_ACCEPT: &str = "accept";
pub const PLAN_DECISION_REVISE: &str = "revise";
pub const PLAN_DECISION_REJECT: &str = "reject";

/// One proposed plan awaiting the human decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub title: String,
    /// The full markdown document under review.
    pub body: String,
}

impl Plan {
    pub fn from_tool_args(args: serde_json::Value) -> ToolResult<Self> {
        let plan: Self =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgument {
                message: format!("invalid plan arguments: {error}"),
            })?;
        plan.validate()?;
        Ok(plan)
    }

    fn validate(&self) -> ToolResult<()> {
        if self.title.trim().is_empty() {
            return Err(ToolError::invalid_argument("plan title must not be empty"));
        }
        if self.title.len() > PLAN_TITLE_MAX_BYTES {
            return Err(ToolError::invalid_argument(format!(
                "plan title exceeds {PLAN_TITLE_MAX_BYTES} bytes"
            )));
        }
        if self.body.trim().is_empty() {
            return Err(ToolError::invalid_argument("plan body must not be empty"));
        }
        if self.body.len() > PLAN_BODY_MAX_BYTES {
            return Err(ToolError::invalid_argument(format!(
                "plan body exceeds {PLAN_BODY_MAX_BYTES} bytes"
            )));
        }
        Ok(())
    }

    /// The durable blocking menu: the markdown body rides `body` line by line
    /// (clients reassemble and render it), the decision vocabulary is fixed
    /// and server-enumerated.
    #[must_use]
    pub fn menu(&self, id: MenuId) -> Menu {
        Menu {
            id,
            kind: MenuKind::Choice,
            title: self.title.clone(),
            body: self.body.lines().map(str::to_owned).collect(),
            options: vec![
                MenuOption {
                    key: PLAN_DECISION_ACCEPT.into(),
                    label: "Accept".into(),
                    detail: Some("approve the plan — the agent proceeds".into()),
                    decision: None,
                },
                MenuOption {
                    key: PLAN_DECISION_REVISE.into(),
                    label: "Revise".into(),
                    detail: Some("send it back with a note — the agent reworks it".into()),
                    decision: None,
                },
                MenuOption {
                    key: PLAN_DECISION_REJECT.into(),
                    label: "Reject".into(),
                    detail: Some("decline the plan — the agent stops this path".into()),
                    decision: None,
                },
            ],
            blocking: true,
            scope: MenuScope::Session,
            origin: PLAN_ORIGIN.into(),
            ttl_ms: None,
            timeout_option: None,
        }
    }

    /// Validates a committed answer against the durable menu. The carrier is
    /// [`RequestInputAnswer`] so the actor's one wait loop serves both tools:
    /// `option_key` is the decision, `value` an optional revise/chat note.
    pub fn resolve(&self, menu: &Menu, answer: &MenuAnswer) -> ToolResult<RequestInputAnswer> {
        if answer.menu != menu.id {
            return Err(ToolError::InvalidMenuAnswer {
                menu: answer.menu.clone(),
                message: format!(
                    "answer targets {}, but open menu is {}",
                    answer.menu, menu.id
                ),
            });
        }
        let option = if let Some(key) = answer.option_key.as_deref() {
            menu.options.iter().find(|option| option.key == key)
        } else {
            usize::try_from(answer.option_index)
                .ok()
                .and_then(|index| menu.options.get(index))
        }
        .ok_or_else(|| ToolError::InvalidMenuAnswer {
            menu: answer.menu.clone(),
            message: "answer does not select a plan decision".into(),
        })?;
        Ok(RequestInputAnswer {
            value: answer.value.clone().unwrap_or_default(),
            option_key: Some(option.key.clone()),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use haider_protocol::menu::AnswerVia;

    fn plan() -> Plan {
        Plan {
            title: "Datacenter proposal".into(),
            body: "# Tiers\n\n- edge\n- core".into(),
        }
    }

    fn answer(menu: &Menu, key: &str, value: Option<&str>) -> MenuAnswer {
        MenuAnswer {
            menu: menu.id.clone(),
            option_key: Some(key.into()),
            option_index: menu
                .options
                .iter()
                .position(|option| option.key == key)
                .map(|index| u32::try_from(index).unwrap())
                .unwrap_or(u32::MAX),
            value: value.map(str::to_owned),
            via: AnswerVia::Tui,
        }
    }

    /// MUTATION CHECK: drop a decision option, change the origin tag, or stop
    /// carrying the markdown body line by line. Expected RUNTIME failure.
    #[test]
    fn plan_menu_carries_document_and_fixed_decisions() {
        let menu = plan().menu(MenuId::new("plan-1"));
        assert_eq!(menu.origin, PLAN_ORIGIN);
        assert_eq!(menu.kind, MenuKind::Choice);
        assert!(menu.blocking);
        assert_eq!(menu.body.len(), 4);
        assert_eq!(menu.body[0], "# Tiers");
        let keys: Vec<_> = menu
            .options
            .iter()
            .map(|option| option.key.as_str())
            .collect();
        assert_eq!(keys, vec!["accept", "revise", "reject"]);
    }

    /// MUTATION CHECK: stop passing the note through, or accept an answer for
    /// a different menu / unknown option. Expected RUNTIME failure.
    #[test]
    fn resolve_maps_decisions_and_carries_the_revise_note() {
        let plan = plan();
        let menu = plan.menu(MenuId::new("plan-2"));
        let accepted = plan.resolve(&menu, &answer(&menu, "accept", None)).unwrap();
        assert_eq!(accepted.option_key.as_deref(), Some("accept"));
        assert!(accepted.value.is_empty());
        let revised = plan
            .resolve(&menu, &answer(&menu, "revise", Some("tighten tier 2")))
            .unwrap();
        assert_eq!(revised.option_key.as_deref(), Some("revise"));
        assert_eq!(revised.value, "tighten tier 2");
        let mut foreign = answer(&menu, "accept", None);
        foreign.menu = MenuId::new("other");
        assert!(plan.resolve(&menu, &foreign).is_err());
        let mut ghost = answer(&menu, "accept", None);
        ghost.option_key = Some("ship".into());
        ghost.option_index = u32::MAX;
        assert!(plan.resolve(&menu, &ghost).is_err());
    }

    /// MUTATION CHECK: drop a validation bound. Expected RUNTIME failure.
    #[test]
    fn args_validation_rejects_empty_and_oversized() {
        assert!(Plan::from_tool_args(serde_json::json!({"title": " ", "body": "x"})).is_err());
        assert!(Plan::from_tool_args(serde_json::json!({"title": "t", "body": ""})).is_err());
        let oversized = "x".repeat(PLAN_BODY_MAX_BYTES + 1);
        assert!(
            Plan::from_tool_args(serde_json::json!({"title": "t", "body": oversized})).is_err()
        );
        assert!(Plan::from_tool_args(serde_json::json!({"title": "t", "body": "# ok"})).is_ok());
    }
}
