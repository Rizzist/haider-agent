//! Typed input and output protocol for the capability-gated `mobile` tool.

use serde::{Deserialize, Serialize};

/// Provider-neutral action vocabulary exposed by Haider's `mobile` tool.
///
/// The top-level action tag matches the existing `computer` tool contract and
/// leaves room for later mobile actions without changing the provider tool
/// name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MobileAction {
    SmsRead {
        folder: Option<String>,
        since: Option<String>,
        limit: Option<u32>,
    },
}

impl MobileAction {
    /// The permission class for this exact dynamic action.
    #[must_use]
    pub const fn effect_class(&self) -> crate::effect::EffectClass {
        match self {
            Self::SmsRead { .. } => crate::effect::EffectClass::ReadSms,
        }
    }
}

/// One SMS returned by a mobile backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmsMessage {
    pub id: String,
    pub address: String,
    pub body: String,
    pub date_ms: i64,
    pub folder: String,
}

/// Provider-neutral result returned by a mobile backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileOutput {
    SmsList(Vec<SmsMessage>),
}

/// Reserved OS-permission vocabulary for future real mobile backends.
///
/// The mock-only SMS read path in this lane has no platform permission park.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobilePermission {
    None,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sms_read_uses_the_read_sms_effect_class() {
        let action = MobileAction::SmsRead {
            folder: Some("inbox".into()),
            since: None,
            limit: Some(2),
        };
        assert_eq!(action.effect_class(), crate::effect::EffectClass::ReadSms);
        assert_eq!(
            serde_json::to_value(crate::effect::EffectClass::ReadSms).expect("effect serializes"),
            serde_json::json!({"class": "read_sms"})
        );
    }

    #[test]
    fn sms_read_wire_shape_is_strict_and_snake_case() {
        let action: MobileAction = serde_json::from_value(serde_json::json!({
            "action": "sms_read",
            "folder": "inbox",
            "since": null,
            "limit": 2
        }))
        .expect("valid sms_read action");
        assert!(matches!(
            action,
            MobileAction::SmsRead {
                folder: Some(folder),
                since: None,
                limit: Some(2)
            } if folder == "inbox"
        ));
        assert!(
            serde_json::from_value::<MobileAction>(serde_json::json!({
                "action": "sms_read",
                "unexpected": true
            }))
            .is_err()
        );
    }
}
