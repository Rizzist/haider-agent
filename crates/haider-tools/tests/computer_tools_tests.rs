#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::computer::ComputerAction;
use haider_protocol::effect::{AuthorizationVerdict, EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::ids::SessionId;
use haider_tools::{
    ALLOW_SCREEN_CONTROL_SESSION_GRANT, ALLOW_SCREEN_SESSION_GRANT, ComputerCancelToken,
    ComputerOperation, EffectBroker, JournalSink, PermissionPolicy, SessionGrant, ToolError,
    ToolResult, computer_manifest,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct SharedJournal(Arc<Mutex<Vec<EventPayload>>>);

#[async_trait]
impl JournalSink for SharedJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.0.lock().expect("journal lock").push(payload);
        Ok(())
    }
}

fn broker() -> (
    tempfile::TempDir,
    EffectBroker,
    Arc<Mutex<Vec<EventPayload>>>,
) {
    let workspace = tempfile::tempdir().expect("workspace");
    let journal = SharedJournal::default();
    let observer = Arc::clone(&journal.0);
    let broker = EffectBroker::new_at(
        Box::new(journal),
        workspace.path(),
        SessionId::new("computer-permission-session"),
        7,
        1_700_000_000_000,
    )
    .expect("computer broker");
    (workspace, broker, observer)
}

fn screenshot() -> ComputerOperation {
    ComputerOperation::new(ComputerAction::Screenshot).expect("screenshot action")
}

fn click() -> ComputerOperation {
    ComputerOperation::new(ComputerAction::LeftClick { x: 10, y: 20 }).expect("click action")
}

#[test]
fn computer_manifest_matches_additive_golden_and_parameter_schemas_are_live() {
    let manifest = computer_manifest();
    assert_eq!(manifest.name, "computer");
    assert_eq!(
        manifest.effects,
        [EffectClass::ScreenObserve, EffectClass::ScreenControl]
    );
    // The checked-in golden predates the additive Linux backend and remains
    // the byte-for-byte macOS contract. Linux advertises its real platform at
    // runtime, then normalizes only this platform-dependent field for the
    // cross-platform schema golden below.
    #[cfg(target_os = "linux")]
    let manifest = {
        let mut manifest = manifest;
        assert_eq!(
            manifest.description,
            "Observe and control the local Linux X11 desktop. Call screenshot before cursor_position or any action with screenshot coordinates."
        );
        manifest.description = "Observe and control the local macOS desktop. Call screenshot before cursor_position or any action with screenshot coordinates.".into();
        manifest
    };
    let serialized = serde_json::to_string_pretty(&manifest).expect("serialize manifest");
    // `include_str!` embeds the fixture with its on-disk line endings; a
    // Windows autocrlf checkout gives it CRLF while serde emits LF. Compare
    // on normalized endings so the golden is content, not whitespace.
    let golden = include_str!("fixtures/computer_manifest.json").replace("\r\n", "\n");
    assert_eq!(serialized, golden.trim_end());
    let schema_text = manifest.input_schema.to_string();
    for unsupported in [
        "oneOf",
        "const",
        "additionalProperties",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
    ] {
        assert!(
            !schema_text.contains(unsupported),
            "provider-common manifest must not contain `{unsupported}`"
        );
    }
    let required = manifest.input_schema["required"]
        .as_array()
        .expect("top-level required fields");
    assert_eq!(required, &[serde_json::json!("action")]);
    assert!(manifest.input_schema["properties"]["from"]["properties"]["x"].is_object());
}

#[test]
fn action_parser_is_strict_and_wait_is_control_gated() {
    let wait = ComputerOperation::from_tool_args(serde_json::json!({
        "action": "wait",
        "ms": 250
    }))
    .expect("wait parses");
    assert_eq!(wait.action().effect_class(), EffectClass::ScreenControl);
    assert!(matches!(
        ComputerOperation::from_tool_args(serde_json::json!({
            "action": "screenshot",
            "future": true
        })),
        Err(ToolError::InvalidArgument { .. })
    ));
    assert!(matches!(
        ComputerOperation::from_tool_args(serde_json::json!({
            "action": "scroll",
            "x": 0,
            "y": 0,
            "direction": "down",
            "amount": 0
        })),
        Err(ToolError::InvalidArgument { .. })
    ));
    assert!(matches!(
        ComputerOperation::from_tool_args(serde_json::json!({
            "action": "left_click_drag",
            "from": {"x": 0, "y": 0, "future": true},
            "to": {"x": 1, "y": 1}
        })),
        Err(ToolError::InvalidArgument { .. })
    ));
}

#[tokio::test]
async fn screen_permissions_fail_closed_and_control_session_grant_implies_observe() {
    assert_eq!(ALLOW_SCREEN_SESSION_GRANT, "allow_screen");
    assert_eq!(ALLOW_SCREEN_CONTROL_SESSION_GRANT, "allow_screen_control");
    let named_observe =
        SessionGrant::for_computer_name(ALLOW_SCREEN_SESSION_GRANT).expect("named observe grant");
    let named_control = SessionGrant::for_computer_name(ALLOW_SCREEN_CONTROL_SESSION_GRANT)
        .expect("named control grant");
    assert_eq!(named_observe.class, EffectClass::ScreenObserve);
    assert_eq!(named_observe.computer_name(), Some("allow_screen"));
    assert_eq!(named_control.class, EffectClass::ScreenControl);
    assert_eq!(named_control.computer_name(), Some("allow_screen_control"));

    let (_workspace, mut default_broker, journal) = broker();
    let denied = default_broker
        .begin_computer(
            &screenshot(),
            &PermissionPolicy::default(),
            ComputerCancelToken::new(),
        )
        .await
        .expect_err("absence of a grant must not dispatch");
    assert!(matches!(denied, ToolError::AuthorizationRequired { .. }));
    let phases = journal
        .lock()
        .expect("journal lock")
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        phases.as_slice(),
        [
            EffectPhase::Intent(_),
            EffectPhase::Authorized {
                verdict: AuthorizationVerdict::Ask { .. },
                ..
            }
        ]
    ));

    let (_workspace, mut observe_broker, _) = broker();
    let mut observe = PermissionPolicy::default();
    observe
        .allow_for_session(EffectClass::ScreenObserve)
        .expect("allow_screen grant");
    let observe_intent = observe_broker
        .begin_computer(&screenshot(), &observe, ComputerCancelToken::new())
        .await
        .expect("allow_screen observes");
    observe_broker
        .journal_outcome(&observe_intent, EffectOutcome::Ok)
        .await
        .expect("observe outcome");
    assert!(matches!(
        observe_broker
            .begin_computer(&click(), &observe, ComputerCancelToken::new())
            .await,
        Err(ToolError::AuthorizationRequired { .. })
    ));

    let (_workspace, mut control_broker, _) = broker();
    let mut control = PermissionPolicy::default();
    control
        .allow_for_session(EffectClass::ScreenControl)
        .expect("allow_screen_control grant");
    let screenshot_intent = control_broker
        .begin_computer(&screenshot(), &control, ComputerCancelToken::new())
        .await
        .expect("control grant implies observe");
    control_broker
        .journal_outcome(&screenshot_intent, EffectOutcome::Ok)
        .await
        .expect("screenshot outcome");
    let click_intent = control_broker
        .begin_computer(&click(), &control, ComputerCancelToken::new())
        .await
        .expect("control grant actuates");
    control_broker
        .journal_outcome(&click_intent, EffectOutcome::Ok)
        .await
        .expect("click outcome");

    control.deny(EffectClass::ScreenObserve, "explicit observe deny");
    assert!(matches!(
        control_broker
            .begin_computer(&screenshot(), &control, ComputerCancelToken::new())
            .await,
        Err(ToolError::PermissionDenied { .. })
    ));
}

#[tokio::test]
async fn ordinary_close_does_not_fabricate_computer_cancellation() {
    let (_workspace, mut broker, journal) = broker();
    let mut policy = PermissionPolicy::default();
    policy
        .allow_for_session(EffectClass::ScreenControl)
        .expect("control grant");
    let token = ComputerCancelToken::new();
    let intent = broker
        .begin_computer(&click(), &policy, token.clone())
        .await
        .expect("computer dispatch");
    broker.close().await.expect("broker closes");

    assert!(!token.is_cancelled());
    assert!(journal.lock().expect("journal lock").iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Effect(EffectPhase::Outcome {
                effect,
                outcome: EffectOutcome::Unknown,
                ..
            }) if effect == &intent.effect
        )
    }));
}
