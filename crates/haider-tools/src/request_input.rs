//! Typed, non-permission input requests.
//!
//! `request_input` deliberately does not enter the effect permission broker:
//! asking a question is not a side effect. The owning actor journals
//! `MenuOpened` and either waits for an interactive answer or applies the
//! session's deterministic autonomous policy. Autonomous mode uses only an
//! explicitly declared default; otherwise it returns `no_human_available`.

use crate::{ToolError, ToolResult};
use haider_protocol::ids::MenuId;
use haider_protocol::menu::{AnswerVia, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestInputKind {
    Question,
    Choice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestInputOption {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestInput {
    pub kind: RequestInputKind,
    pub title: String,
    #[serde(default)]
    pub body: Vec<String>,
    #[serde(default)]
    pub options: Vec<RequestInputOption>,
    /// Declared deterministic fallback for a session with no human. For a
    /// question this is the literal answer; for a choice it is an option key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestInputAnswer {
    pub value: String,
    pub option_key: Option<String>,
}

impl RequestInput {
    pub fn new(
        kind: RequestInputKind,
        title: impl Into<String>,
        options: Vec<RequestInputOption>,
    ) -> ToolResult<Self> {
        let request = Self {
            kind,
            title: title.into(),
            body: Vec::new(),
            options,
            default: None,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn from_tool_args(args: serde_json::Value) -> ToolResult<Self> {
        let request: Self =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgument {
                message: format!("invalid request_input arguments: {error}"),
            })?;
        request.validate()?;
        Ok(request)
    }

    pub fn menu(&self, id: MenuId) -> Menu {
        Menu {
            id,
            kind: match self.kind {
                RequestInputKind::Question => MenuKind::Question,
                RequestInputKind::Choice => MenuKind::Choice,
            },
            title: self.title.clone(),
            body: self.body.clone(),
            options: self
                .options
                .iter()
                .map(|option| MenuOption {
                    key: option.key.clone(),
                    label: option.label.clone(),
                    detail: option.detail.clone(),
                    decision: None,
                })
                .collect(),
            blocking: true,
            scope: MenuScope::Session,
            origin: "request_input".into(),
            ttl_ms: None,
            timeout_option: None,
        }
    }

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
        match self.kind {
            RequestInputKind::Question => {
                if let Some(value) = answer.value.as_ref().filter(|value| !value.is_empty()) {
                    return Ok(RequestInputAnswer {
                        value: value.clone(),
                        option_key: None,
                    });
                }
                self.resolve_option(menu, answer)
            }
            RequestInputKind::Choice => self.resolve_option(menu, answer),
        }
    }

    #[must_use]
    pub const fn has_declared_default(&self) -> bool {
        self.default.is_some()
    }

    /// Produces the same typed menu answer an interactive surface would have
    /// sent, but only from the request's explicit default.
    pub fn declared_default_answer(&self, menu: &Menu) -> ToolResult<MenuAnswer> {
        let default = self
            .default
            .as_deref()
            .ok_or_else(|| ToolError::invalid_argument("request_input has no declared default"))?;
        match self.kind {
            RequestInputKind::Question => Ok(MenuAnswer {
                menu: menu.id.clone(),
                option_key: None,
                option_index: 0,
                value: Some(default.to_owned()),
                via: AnswerVia::Hook,
            }),
            RequestInputKind::Choice => {
                let (index, option) = menu
                    .options
                    .iter()
                    .enumerate()
                    .find(|(_, option)| option.key == default)
                    .ok_or_else(|| ToolError::InvalidMenuAnswer {
                        menu: menu.id.clone(),
                        message: "request_input default does not name a declared option".into(),
                    })?;
                Ok(MenuAnswer {
                    menu: menu.id.clone(),
                    option_key: Some(option.key.clone()),
                    option_index: u32::try_from(index).map_err(|_| {
                        ToolError::InvalidMenuAnswer {
                            menu: menu.id.clone(),
                            message: "request_input default index exceeds protocol bounds".into(),
                        }
                    })?,
                    value: None,
                    via: AnswerVia::Hook,
                })
            }
        }
    }

    fn validate(&self) -> ToolResult<()> {
        if self.title.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "request_input title must not be empty",
            ));
        }
        if self.kind == RequestInputKind::Choice && self.options.is_empty() {
            return Err(ToolError::invalid_argument(
                "choice request_input requires at least one server-enumerated option",
            ));
        }
        if self.default.as_ref().is_some_and(|value| value.is_empty()) {
            return Err(ToolError::invalid_argument(
                "request_input default must not be empty",
            ));
        }
        let mut keys = std::collections::HashSet::new();
        for option in &self.options {
            if option.key.trim().is_empty() || option.label.trim().is_empty() {
                return Err(ToolError::invalid_argument(
                    "request_input option keys and labels must not be empty",
                ));
            }
            if !keys.insert(option.key.as_str()) {
                return Err(ToolError::invalid_argument(format!(
                    "duplicate request_input option key `{}`",
                    option.key
                )));
            }
        }
        if self.kind == RequestInputKind::Choice
            && let Some(default) = self.default.as_deref()
            && !keys.contains(default)
        {
            return Err(ToolError::invalid_argument(
                "choice request_input default must name a declared option key",
            ));
        }
        Ok(())
    }

    fn resolve_option(&self, menu: &Menu, answer: &MenuAnswer) -> ToolResult<RequestInputAnswer> {
        let option = if let Some(key) = answer.option_key.as_deref() {
            menu.options.iter().find(|option| option.key == key)
        } else {
            usize::try_from(answer.option_index)
                .ok()
                .and_then(|index| menu.options.get(index))
        }
        .ok_or_else(|| ToolError::InvalidMenuAnswer {
            menu: answer.menu.clone(),
            message: "answer does not select a server-enumerated option".into(),
        })?;
        Ok(RequestInputAnswer {
            value: option.label.clone(),
            option_key: Some(option.key.clone()),
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn option(key: &str, label: &str) -> RequestInputOption {
        RequestInputOption {
            key: key.into(),
            label: label.into(),
            detail: None,
        }
    }

    #[test]
    fn declared_question_default_resolves_without_invention() {
        let request = RequestInput::from_tool_args(serde_json::json!({
            "kind": "question",
            "title": "Branch name",
            "default": "main"
        }))
        .expect("valid request");
        let menu = request.menu(MenuId::new("question-default"));
        let answer = request
            .declared_default_answer(&menu)
            .expect("default answer");
        assert_eq!(answer.value.as_deref(), Some("main"));
        assert_eq!(
            request.resolve(&menu, &answer).expect("resolved").value,
            "main"
        );
    }

    #[test]
    fn declared_choice_default_names_an_existing_key() {
        let request = RequestInput::from_tool_args(serde_json::json!({
            "kind": "choice",
            "title": "Target",
            "options": [
                {"key": "library", "label": "Library"},
                {"key": "binary", "label": "Binary"}
            ],
            "default": "binary"
        }))
        .expect("valid request");
        let menu = request.menu(MenuId::new("choice-default"));
        let answer = request
            .declared_default_answer(&menu)
            .expect("default answer");
        let resolved = request.resolve(&menu, &answer).expect("resolved");
        assert_eq!(resolved.option_key.as_deref(), Some("binary"));
        assert_eq!(resolved.value, "Binary");
    }

    #[test]
    fn malformed_defaults_are_rejected_instead_of_guessed() {
        assert!(
            RequestInput::from_tool_args(serde_json::json!({
                "kind": "question", "title": "Name", "default": ""
            }))
            .is_err()
        );
        let request = RequestInput {
            kind: RequestInputKind::Choice,
            title: "Target".into(),
            body: Vec::new(),
            options: vec![option("library", "Library")],
            default: Some("binary".into()),
        };
        assert!(request.validate().is_err());
    }
}
