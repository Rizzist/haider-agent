//! The generic `plan` tool — present-and-proceed for any deliverable.
//!
//! Like `request_input`, `plan` deliberately never enters the effect
//! permission broker: presenting a proposal is not a side effect. The owning
//! actor journals `MenuOpened`/`MenuAnswered` and immediately returns the
//! automatic acceptance as the tool result. Unlike `request_input`, a plan
//! never parks the run or waits for a human. Its payload remains a full
//! markdown document (an architecture proposal, a migration plan, an
//! agent-type design) carried in the durable menu body for clients to render.

use crate::{ToolError, ToolResult};
use haider_protocol::ids::MenuId;
use haider_protocol::menu::{AnswerVia, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use serde::{Deserialize, Serialize};

/// Menu origin tag clients key their plan surfaces on.
pub const PLAN_ORIGIN: &str = "plan";
/// Title is a headline, not a document.
pub const PLAN_TITLE_MAX_BYTES: usize = 120;
/// The markdown body rides the durable menu; bounded like other durable text.
pub const PLAN_BODY_MAX_BYTES: usize = 32 * 1024;

pub const PLAN_DECISION_ACCEPT: &str = "accept";

/// One plan document presented before the agent proceeds autonomously.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub title: String,
    /// The full markdown document presented to clients.
    pub body: String,
}

/// The fixed provider-facing result of presenting a valid plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanResult {
    pub decision: String,
    pub note: String,
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

    /// The durable non-blocking presentation: the markdown body rides `body`
    /// line by line for clients to reassemble and render. The sole option is
    /// the actor-owned automatic settlement target, not a human decision.
    #[must_use]
    pub fn menu(&self, id: MenuId) -> Menu {
        Menu {
            id,
            kind: MenuKind::Choice,
            title: self.title.clone(),
            body: self.body.lines().map(str::to_owned).collect(),
            options: vec![MenuOption {
                key: PLAN_DECISION_ACCEPT.into(),
                label: "Proceeding automatically".into(),
                detail: Some("the plan is recorded and the agent continues immediately".into()),
                decision: None,
            }],
            blocking: false,
            scope: MenuScope::Session,
            origin: PLAN_ORIGIN.into(),
            ttl_ms: None,
            timeout_option: None,
        }
    }

    /// Builds the actor-owned durable settlement for this presentation.
    /// There is deliberately no API for resolving a human decision.
    pub fn automatic_answer(&self, menu: &Menu) -> ToolResult<MenuAnswer> {
        let Some((index, option)) = menu
            .options
            .iter()
            .enumerate()
            .find(|(_, option)| option.key == PLAN_DECISION_ACCEPT)
        else {
            return Err(ToolError::InvalidMenuAnswer {
                menu: menu.id.clone(),
                message: "plan presentation has no automatic accept settlement".into(),
            });
        };
        let option_index = u32::try_from(index).map_err(|_| ToolError::InvalidMenuAnswer {
            menu: menu.id.clone(),
            message: "plan acceptance index exceeds protocol bounds".into(),
        })?;
        Ok(MenuAnswer {
            menu: menu.id.clone(),
            option_key: Some(option.key.clone()),
            option_index,
            value: None,
            via: AnswerVia::Hook,
        })
    }

    /// Every valid plan call has the same immediate provider-facing result.
    #[must_use]
    pub fn accepted_result(&self) -> PlanResult {
        PlanResult {
            decision: PLAN_DECISION_ACCEPT.into(),
            note: String::new(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            title: "Datacenter proposal".into(),
            body: "# Tiers\n\n- edge\n- core".into(),
        }
    }

    /// MUTATION CHECK: make the presentation blocking, expose a human
    /// decision, change the origin tag, or drop the markdown body. Expected
    /// RUNTIME failure.
    #[test]
    fn plan_presentation_is_nonblocking_and_auto_accepts() {
        let plan = plan();
        let menu = plan.menu(MenuId::new("plan-1"));
        assert_eq!(menu.origin, PLAN_ORIGIN);
        assert_eq!(menu.kind, MenuKind::Choice);
        assert!(!menu.blocking);
        assert_eq!(menu.body.len(), 4);
        assert_eq!(menu.body[0], "# Tiers");
        let keys: Vec<_> = menu
            .options
            .iter()
            .map(|option| option.key.as_str())
            .collect();
        assert_eq!(keys, vec!["accept"]);

        let answer = plan.automatic_answer(&menu).unwrap();
        assert_eq!(answer.menu, menu.id);
        assert_eq!(answer.option_key.as_deref(), Some("accept"));
        assert_eq!(answer.option_index, 0);
        assert_eq!(answer.via, AnswerVia::Hook);

        assert_eq!(
            plan.accepted_result(),
            PlanResult {
                decision: "accept".into(),
                note: String::new(),
            }
        );
    }

    /// MUTATION CHECK: drop a validation bound. Expected RUNTIME failure.
    #[test]
    fn args_validation_rejects_empty_and_oversized() {
        assert!(Plan::from_tool_args(serde_json::json!({"title": " ", "body": "x"})).is_err());
        assert!(Plan::from_tool_args(serde_json::json!({"title": "t", "body": ""})).is_err());
        let oversized_title = "x".repeat(PLAN_TITLE_MAX_BYTES + 1);
        assert!(
            Plan::from_tool_args(serde_json::json!({"title": oversized_title, "body": "x"}))
                .is_err()
        );
        let oversized = "x".repeat(PLAN_BODY_MAX_BYTES + 1);
        assert!(
            Plan::from_tool_args(serde_json::json!({"title": "t", "body": oversized})).is_err()
        );
        assert!(Plan::from_tool_args(serde_json::json!({"title": "t", "body": "# ok"})).is_ok());
    }
}
