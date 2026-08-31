//! W-B `web_fetch` — the universal LOCAL web-fetch client tool.
//!
//! The operation here is the PERMISSION half: it normalizes the model's
//! `{url, max_bytes?}` arguments into a `Network { host }` effect whose
//! intent and outcome journal with the URL (LW7). The guarded HTTP engine
//! (strict-public origin policy, redirect re-validation, html reduction,
//! output cap) lives daemon-side behind the broker's begin/outcome seam —
//! the `spawn_subagent` begin/finish precedent.

use haider_protocol::effect::EffectClass;
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde_json::Value;

use crate::broker::EffectOperation;
use crate::{ToolError, ToolResult};

/// Hard cap the manifest documents for one fetch result.
pub const WEB_FETCH_TOOL_OUTPUT_CAP_BYTES: u64 = 96 * 1024;
/// Smallest caller-selected cap that can contain a machine-readable elision
/// record while retaining useful head and tail content.
pub const WEB_FETCH_TOOL_MIN_OUTPUT_BYTES: u64 = 512;

/// One validated `web_fetch` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFetch {
    url: String,
    host: String,
    max_bytes: Option<u64>,
}

impl WebFetch {
    /// Validates the model-supplied arguments far enough to mint an honest
    /// permission key: an absolute http(s) URL with a host. The full
    /// strict-public origin policy runs at execution — this is the
    /// PERMISSION shape, not the network fence.
    pub fn new(url: impl Into<String>, max_bytes: Option<u64>) -> ToolResult<Self> {
        let url = url.into();
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return Err(ToolError::invalid_argument(
                "web_fetch requires a non-empty `url`",
            ));
        }
        let (scheme, rest) = trimmed
            .split_once("://")
            .ok_or_else(|| ToolError::invalid_argument("web_fetch `url` must be absolute"))?;
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Err(ToolError::invalid_argument(format!(
                "web_fetch supports only http(s) URLs, not `{scheme}`"
            )));
        }
        let authority = rest
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .trim();
        if authority.contains('@') {
            return Err(ToolError::invalid_argument(
                "web_fetch URLs must not carry userinfo",
            ));
        }
        let host = if let Some(bracketed) = authority.strip_prefix('[') {
            let end = bracketed.find(']').ok_or_else(|| {
                ToolError::invalid_argument("web_fetch `url` has an unterminated IPv6 host")
            })?;
            format!("[{}]", &bracketed[..end])
        } else {
            authority
                .rsplit_once(':')
                .map_or(authority, |(host, _port)| host)
                .to_owned()
        };
        if host.is_empty() {
            return Err(ToolError::invalid_argument("web_fetch `url` has no host"));
        }
        if max_bytes.is_some_and(|bytes| bytes < WEB_FETCH_TOOL_MIN_OUTPUT_BYTES) {
            return Err(ToolError::invalid_argument(format!(
                "web_fetch `max_bytes` must be at least {WEB_FETCH_TOOL_MIN_OUTPUT_BYTES} so a truncation marker fits"
            )));
        }
        Ok(Self {
            url: trimmed.to_owned(),
            host: host.to_ascii_lowercase(),
            max_bytes,
        })
    }

    /// Parses the tool-call arguments object.
    pub fn from_tool_args(args: &Value) -> ToolResult<Self> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::invalid_argument("web_fetch requires a string `url`"))?;
        let max_bytes = match args.get("max_bytes") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                ToolError::invalid_argument("web_fetch `max_bytes` must be a positive integer")
            })?),
        };
        Self::new(url, max_bytes)
    }

    /// The permission-relevant host, exactly as brokered.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    #[must_use]
    pub fn max_bytes(&self) -> Option<u64> {
        self.max_bytes
    }
}

impl EffectOperation for WebFetch {
    fn effect_class(&self) -> EffectClass {
        EffectClass::Network {
            host: self.host.clone(),
        }
    }

    fn summary(&self) -> String {
        format!("fetch {}", self.url)
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(serde_json::json!({
            "url": self.url,
            "max_bytes": self.max_bytes,
        }))
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![
            format!("fetch {}", self.url),
            "public https (or loopback http) only; redirects re-checked per hop".to_owned(),
        ]
    }
}

/// Registry manifest for the CLIENT `web_search` tool (W-B decision 3):
/// advertised on responses-lite pairs only, executed daemon-side against the
/// codex alpha/search endpoint with the SAME subscription credential as
/// turns — provider-credential traffic, not a brokered effect (the
/// `request_input` pattern).
#[must_use]
pub fn web_search_manifest() -> ToolManifest {
    ToolManifest {
        name: "web_search".into(),
        description: "Search the web and return a bounded text summary of the results.".into(),
        effects: Vec::new(),
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query.",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    }
}

/// Registry manifest for the universal local `web_fetch` client tool.
#[must_use]
pub fn web_fetch_manifest() -> ToolManifest {
    ToolManifest {
        name: "web_fetch".into(),
        description: format!(
            "Fetch a public https URL (or loopback http) and return its readable text. \
             HTML is reduced to text; only text/* and application/json bodies are \
             supported; output is capped at {WEB_FETCH_TOOL_OUTPUT_CAP_BYTES} bytes \
             with an honest truncation marker."
        ),
        effects: vec![EffectClass::Network {
            host: String::new(),
        }],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL to fetch.",
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": WEB_FETCH_TOOL_MIN_OUTPUT_BYTES,
                    "description": "Optional smaller output cap in bytes.",
                },
            },
            "required": ["url"],
            "additionalProperties": false,
        }),
    }
}
