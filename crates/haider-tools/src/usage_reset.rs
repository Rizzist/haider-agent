use haider_protocol::effect::EffectClass;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};

/// Request or poll a provider-issued usage reset. `approval_id` is absent on
/// the first call and present when polling a previously parked request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageResetApply {
    pub account: CredentialAlias,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
}

pub fn usage_reset_apply_manifest() -> ToolManifest {
    ToolManifest {
        name: "usage_reset_apply".into(),
        description: "Request use of one provider-issued rate-limit reset credit, or poll a prior request. A new request always returns approval_pending immediately and requires a human decision; it never blocks the agent and never inherits auto-allow/bypass mode."
            .into(),
        effects: vec![EffectClass::UsageResetApply],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "account": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Credential alias from usage.report"
                },
                "approval_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Optional approval id returned by an earlier call; when present, poll instead of opening another approval"
                }
            },
            "required": ["account"],
            "additionalProperties": false
        }),
    }
}
