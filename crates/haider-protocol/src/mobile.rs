//! Typed input and output protocol for the capability-gated `mobile` tool.

use serde::{Deserialize, Serialize};

/// One point in the mobile screenshot coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Rectangle edges in the mobile screenshot coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point4 {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Named mobile navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileKey {
    Back,
    Home,
    Enter,
    Recents,
}

/// Provider-neutral action vocabulary exposed by Haider's `mobile` tool.
///
/// The top-level action tag matches the existing `computer` tool contract so
/// provider adapters can drive a screenshot/accessibility see-act loop without
/// introducing a second command envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MobileAction {
    Screenshot {},
    A11yTree {},
    Inspect {
        element_id: Option<String>,
        x: Option<i32>,
        y: Option<i32>,
    },
    Tap {
        element_id: Option<String>,
        x: Option<i32>,
        y: Option<i32>,
    },
    LongPress {
        element_id: Option<String>,
        x: Option<i32>,
        y: Option<i32>,
    },
    Swipe {
        from: Point,
        to: Point,
    },
    Type {
        text: String,
    },
    Key {
        key: MobileKey,
    },
    OpenApp {
        package: Option<String>,
        name: Option<String>,
    },
    ListApps {},
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
            Self::Screenshot {} | Self::A11yTree {} | Self::Inspect { .. } | Self::ListApps {} => {
                crate::effect::EffectClass::MobileObserve
            }
            Self::Tap { .. }
            | Self::LongPress { .. }
            | Self::Swipe { .. }
            | Self::Type { .. }
            | Self::Key { .. }
            | Self::OpenApp { .. } => crate::effect::EffectClass::MobileControl,
            Self::SmsRead { .. } => crate::effect::EffectClass::ReadSms,
        }
    }
}

/// One accessibility node returned by the mobile backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct A11yNode {
    pub id: String,
    pub text: Option<String>,
    pub content_desc: Option<String>,
    pub class: String,
    pub resource_id: Option<String>,
    pub bounds: Point4,
}

/// One installed application returned by `list_apps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppEntry {
    pub package: String,
    pub name: String,
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

/// Provider-neutral result returned by a mobile backend before the daemon
/// admits screenshot bytes into CU-1/CAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileOutput {
    Screenshot(Vec<u8>),
    A11yTree(Vec<A11yNode>),
    AppList(Vec<AppEntry>),
    Ack,
    SmsList(Vec<SmsMessage>),
}

/// Reserved OS-permission vocabulary for future real mobile backends.
///
/// The mock-only mobile path in this lane has no platform permission park.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobilePermission {
    None,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::effect::EffectClass;

    #[test]
    fn actions_use_their_exact_effect_classes() {
        for action in [
            MobileAction::Screenshot {},
            MobileAction::A11yTree {},
            MobileAction::Inspect {
                element_id: Some("send".into()),
                x: None,
                y: None,
            },
        ] {
            assert_eq!(action.effect_class(), EffectClass::MobileObserve);
        }
        for action in [
            MobileAction::Tap {
                element_id: None,
                x: Some(10),
                y: Some(20),
            },
            MobileAction::Swipe {
                from: Point { x: 0, y: 1 },
                to: Point { x: 2, y: 3 },
            },
            MobileAction::Type { text: "hi".into() },
            MobileAction::Key {
                key: MobileKey::Back,
            },
            MobileAction::OpenApp {
                package: Some("com.example".into()),
                name: None,
            },
        ] {
            assert_eq!(action.effect_class(), EffectClass::MobileControl);
        }
        let sms = MobileAction::SmsRead {
            folder: Some("inbox".into()),
            since: None,
            limit: Some(2),
        };
        assert_eq!(sms.effect_class(), EffectClass::ReadSms);
        assert_eq!(
            serde_json::to_value(EffectClass::MobileObserve).expect("effect serializes"),
            serde_json::json!({"class": "mobile_observe"})
        );
        assert_eq!(
            serde_json::to_value(EffectClass::MobileControl).expect("effect serializes"),
            serde_json::json!({"class": "mobile_control"})
        );
    }

    #[test]
    fn mobile_wire_shapes_are_strict_and_snake_case() {
        let action: MobileAction = serde_json::from_value(serde_json::json!({
            "action": "swipe",
            "from": {"x": 1, "y": 2},
            "to": {"x": 3, "y": 4}
        }))
        .expect("valid swipe action");
        assert_eq!(
            action,
            MobileAction::Swipe {
                from: Point { x: 1, y: 2 },
                to: Point { x: 3, y: 4 },
            }
        );
        assert!(
            serde_json::from_value::<MobileAction>(serde_json::json!({
                "action": "screenshot",
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<MobileAction>(serde_json::json!({
                "action": "swipe",
                "from": {"x": 1, "y": 2, "z": 3},
                "to": {"x": 3, "y": 4}
            }))
            .is_err()
        );
    }
}
