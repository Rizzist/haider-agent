//! Typed, non-permission input requests.
//!
//! `request_input` deliberately does not enter the effect permission broker:
//! asking a question is not a side effect. The owning actor journals
//! `MenuOpened` and `MenuAnswered`, parks the run in `InputRequired`, and
//! returns the validated answer as the tool result.

use crate::{ToolError, ToolResult};
use haider_protocol::ids::MenuId;
use haider_protocol::menu::{Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
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
