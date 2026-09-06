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
    // The description names the RUNNING platform's desktop (macOS / Linux
    // X11+Wayland / Windows), so it is inherently target-specific. Assert it
    // is a real platform description, then normalize it to the golden's value
    // so the byte comparison below validates the SCHEMA/structure — which is
    // platform-identical — rather than the per-OS wording.
    let manifest = {
        let mut manifest = manifest;
        assert!(
            manifest.description.contains("desktop")
                && manifest.description.ends_with("screenshot coordinates."),
            "description names a platform desktop: {}",
            manifest.description
        );
        manifest.description = "Observe and control the local macOS desktop. Call screenshot before cursor_position or any action with screenshot coordinates.".into();
        manifest
    };
    let serialized = serde_json::to_string_pretty(&manifest).expect("serialize manifest");
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::write(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/computer_manifest.json"),
            format!("{serialized}\n"),
        )
        .expect("regenerate computer manifest fixture");
        return;
    }
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
    let inspect = ComputerOperation::from_tool_args(serde_json::json!({
        "action": "inspect",
        "x": 30,
        "y": 40
    }))
    .expect("inspect parses");
    assert_eq!(inspect.action().effect_class(), EffectClass::ScreenObserve);
    let wait = ComputerOperation::from_tool_args(serde_json::json!({
        "action": "wait",
        "ms": 250
    }))
    .expect("wait parses");
    assert_eq!(wait.action().effect_class(), EffectClass::ScreenControl);
    assert!(matches!(
        ComputerOperation::from_tool_args(serde_json::json!({
            "action": "inspect",
            "x": 30,
            "y": 40,
            "future": true
        })),
        Err(ToolError::InvalidArgument { .. })
    ));
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

#[test]
fn triple_click_is_additive_control_gated_and_rejects_coordinates() {
    let operation = ComputerOperation::from_tool_args(serde_json::json!({"action":"triple_click"}))
        .expect("triple click parses");
    assert_eq!(operation.action(), &ComputerAction::TripleClick);
    assert_eq!(operation.action().click_count(), Some(3));
    assert_eq!(ComputerAction::DoubleClick.click_count(), Some(2));
    assert_eq!(ComputerAction::Screenshot.click_count(), None);
    assert_eq!(
        operation.action().effect_class(),
        EffectClass::ScreenControl
    );
    for invalid in [
        serde_json::json!({"action":"triple_click", "x": 1, "y": 2}),
        serde_json::json!({"action":"triple_click", "count": 4}),
    ] {
        assert!(ComputerOperation::from_tool_args(invalid).is_err());
    }
    let encoded = serde_json::to_value(operation.action()).expect("encode action");
    assert_eq!(encoded, serde_json::json!({"action":"triple_click"}));
}

#[test]
fn key_chords_keep_modifier_combos_in_the_neutral_contract() {
    for keys in ["cmd+shift+4", "ctrl+alt+delete", "alt+tab", "shift+left"] {
        let operation =
            ComputerOperation::from_tool_args(serde_json::json!({"action":"key", "keys":keys}))
                .expect("modifier chord parses");
        assert_eq!(
            operation.action(),
            &ComputerAction::Key { keys: keys.into() }
        );
        assert_eq!(
            operation.action().effect_class(),
            EffectClass::ScreenControl
        );
    }
    for invalid in [
        serde_json::json!({"action":"key", "keys":" "}),
        serde_json::json!({"action":"key", "keys":"cmd++a"}),
        serde_json::json!({"action":"key", "keys":"unknown+a"}),
        serde_json::json!({"action":"key", "keys":"cmd+"}),
        serde_json::json!({"action":"key", "keys":["cmd", "a"]}),
        serde_json::json!({"action":"key"}),
    ] {
        assert!(ComputerOperation::from_tool_args(invalid).is_err());
    }
}

#[test]
fn screenshot_region_arguments_validate_and_broker_roundtrip() {
    use haider_tools::EffectOperation;
    let args = serde_json::json!({"action":"screenshot","region":{"x":10,"y":20,"width":30,"height":40,"reference_width":100,"reference_height":100}});
    let operation = ComputerOperation::from_tool_args(args.clone()).expect("region");
    assert_eq!(operation.arguments().expect("broker arguments"), args);
    assert_eq!(operation.action(), &ComputerAction::Screenshot);
    assert!(operation.region().is_some());
    for field in ["width", "height", "reference_width", "reference_height"] {
        let mut invalid = args.clone();
        invalid["region"][field] = serde_json::json!(0);
        assert!(
            ComputerOperation::from_tool_args(invalid).is_err(),
            "{field}"
        );
    }
    for invalid_region in [
        serde_json::json!({"x":0}),
        serde_json::json!(null),
        serde_json::json!({"x":0,"y":0,"width":1,"height":1,"reference_width":1,"reference_height":1,"extra":true}),
    ] {
        let mut invalid = args.clone();
        invalid["region"] = invalid_region;
        assert!(ComputerOperation::from_tool_args(invalid).is_err());
    }
    let mut invalid = args;
    invalid["action"] = serde_json::json!("cursor_position");
    assert!(ComputerOperation::from_tool_args(invalid).is_err());
}
